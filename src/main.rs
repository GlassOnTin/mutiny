use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

use mutiny::action::{ACTIONS, build_menubar_model, by_slug};
use mutiny::app::AppController;
use mutiny::config::{self, Overrides};
use mutiny::http_mcp::{self, HttpMcpConfig};
use mutiny::ipc::{
    ConsentMode, IpcRequest, IpcResponse, agent_socket_path, default_audit_path,
    default_socket_path, send_request,
};
use mutiny::mcp::McpBridge;
use mutiny::topology::{PaneId, SplitAxis, TabId, WindowId};

#[derive(Debug, Parser)]
#[command(
    name = "mutiny",
    version,
    about = "Pure Rust + GTK4 Linux desktop terminal with pane mobility and shared human+agent presence"
)]
struct Cli {
    /// Override Unix control socket path (defaults to $XDG_RUNTIME_DIR/mutiny.sock)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    /// Working directory for initial shell
    #[arg(short = 'd', long)]
    working_directory: Option<PathBuf>,

    /// Override shell command to launch (e.g. --shell /bin/bash)
    #[arg(long)]
    shell: Option<String>,

    /// GUI only: consent mode the agent carrier assigns to MCP connections.
    /// This is the operator-side grant; a bridge cannot raise it. `paired`
    /// allows Tier-2 acts without a prompt, so opt in deliberately.
    #[arg(long, value_enum, default_value_t = McpConsentArg::Ask)]
    agent_consent: McpConsentArg,

    /// GUI only: font family, overriding config.toml [font] family.
    #[arg(long)]
    font: Option<String>,

    /// GUI only: font size in points, overriding config.toml [font] size.
    #[arg(long)]
    font_size: Option<u32>,

    /// GUI only: initial pane columns, overriding config.toml [geometry].
    #[arg(long)]
    cols: Option<usize>,

    /// GUI only: initial pane rows, overriding config.toml [geometry].
    #[arg(long)]
    rows: Option<usize>,

    /// GUI only: start empty instead of restoring the saved session.
    #[arg(long)]
    no_restore: bool,

    /// GUI only: serve authenticated MCP over HTTP on this address. Off by
    /// default. Name an explicit interface (a WireGuard or SSH-tunneled IP);
    /// a wildcard bind is refused. Requires a pairing token.
    #[arg(long)]
    http_bind: Option<std::net::SocketAddr>,

    /// GUI only: pairing token for --http-bind. Generated and printed once if
    /// omitted. Clients send it as `Authorization: Bearer <token>`.
    #[arg(long)]
    http_token: Option<String>,

    #[command(subcommand)]
    command: Option<TopCommand>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum McpConsentArg {
    /// Tier-1 Observe free; Tier-2 Act prompts operator in Mutiny window chrome
    Ask,
    /// Paired trusted peer (e.g. Haven over WireGuard/SSH); Tier-1 & Tier-2 allowed and audited
    Paired,
    /// Strictly read-only; Tier-2 Act verbs are rejected
    ObserveOnly,
}

impl From<McpConsentArg> for ConsentMode {
    fn from(val: McpConsentArg) -> Self {
        match val {
            McpConsentArg::Ask => ConsentMode::Ask,
            McpConsentArg::Paired => ConsentMode::Paired,
            McpConsentArg::ObserveOnly => ConsentMode::ObserveOnly,
        }
    }
}

#[derive(Debug, Subcommand)]
enum TopCommand {
    /// Control a running Mutiny instance over its Unix domain socket
    Ctl {
        #[command(subcommand)]
        verb: CtlVerb,
    },
    /// Run a stdio JSON-RPC 2.0 MCP server bridging tools and `ui://mutiny/*` resources
    Mcp {
        #[arg(long, value_enum, default_value_t = McpConsentArg::Ask)]
        consent: McpConsentArg,
        #[arg(long, default_value = "mcp-client")]
        client_name: String,
    },
}

#[derive(Debug, Subcommand)]
enum CtlVerb {
    /// List windows, tabs, split trees, and pane states as JSON
    List,
    /// List every registered ActionSpec verb, shortcut, consent tier, and slug
    Actions,
    /// Inspect redacted agent/IPC audit history
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Read structured terminal snapshot (cursor, modes, OSC state, trimmed lines)
    Snapshot {
        #[arg(long)]
        pane: Option<u64>,
        #[arg(long)]
        max_lines: Option<usize>,
    },
    /// Read final screen and exit code of recently closed or fast-exited panes
    ReadExited {
        #[arg(long)]
        pane: Option<u64>,
    },
    /// Search a pane's visible and scrollback lines
    Search {
        pattern: String,
        #[arg(long)]
        pane: Option<u64>,
        #[arg(long, default_value_t = 20)]
        max_matches: usize,
    },
    /// Trigger any ActionSpec verb by slug (e.g. `detach-split`, `urgent-next`)
    Run {
        slug: String,
        #[arg(long)]
        window: Option<u64>,
    },
    /// Open a top-level terminal window
    SpawnWindow {
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Open a tab in the active (or specified) window
    SpawnTab {
        #[arg(long)]
        window: Option<u64>,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Split the active (or specified) pane right (default) or down (`--down`)
    Split {
        #[arg(long)]
        pane: Option<u64>,
        #[arg(long)]
        down: bool,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Focus a specific pane across windows and tabs
    Focus { pane: u64 },
    /// Detach a split pane into its own top-level window
    DetachSplit {
        #[arg(long)]
        pane: Option<u64>,
    },
    /// Move a pane into another tab
    MovePane {
        pane: u64,
        target_tab: u64,
        #[arg(long)]
        down: bool,
    },
    /// Write raw text bytes into a pane's PTY
    SendText {
        text: String,
        #[arg(long)]
        pane: Option<u64>,
    },
    /// Send text (optionally bracketed) followed by named keys (`enter`, `esc`, `ctrl-c`, arrows)
    SendInput {
        #[arg(long)]
        pane: Option<u64>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long = "key")]
        keys: Vec<String>,
        #[arg(long)]
        bracketed_paste: bool,
        #[arg(long)]
        return_snapshot: bool,
    },
    /// Deliver one submitted turn to a raw-mode REPL or shell and return snapshot + sequence ids
    SendToAgent {
        message: String,
        #[arg(long)]
        pane: Option<u64>,
        #[arg(long)]
        max_lines: Option<usize>,
    },
    /// Post or clear an operator notice banner + attention badge on a pane
    PresentNotice {
        #[arg(long)]
        pane: Option<u64>,
        message: Option<String>,
    },
    /// Place an image file inline in a pane via Kitty graphics
    PresentMedia {
        path: PathBuf,
        #[arg(long)]
        pane: Option<u64>,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
    },
    /// Read the captured output and exit code of the last OSC 133 command
    LastOutput {
        #[arg(long)]
        pane: Option<u64>,
    },
    /// Read the visible viewport text of a pane
    Screen {
        #[arg(long)]
        pane: Option<u64>,
    },
    /// Block until the next OSC 133 prompt arrives on the target pane
    Wait {
        #[arg(long)]
        pane: Option<u64>,
        #[arg(long)]
        since_seq: Option<u64>,
        #[arg(long, default_value_t = 15_000)]
        timeout_ms: u64,
    },
    /// Wait until either OSC 133 prompt arrives or REPL output settles for `settle_ms`
    AwaitTurn {
        #[arg(long)]
        pane: Option<u64>,
        #[arg(long)]
        since_prompt_seq: Option<u64>,
        #[arg(long)]
        since_output_seq: Option<u64>,
        #[arg(long, default_value_t = 250)]
        settle_ms: u64,
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let explicit_socket = cli.socket.clone();
    let operator_path = explicit_socket.clone().unwrap_or_else(default_socket_path);

    match cli.command {
        Some(TopCommand::Ctl { verb }) => run_ctl(&operator_path, verb),
        Some(TopCommand::Mcp {
            consent,
            client_name,
        }) => {
            // Agents arrive on the agent carrier, never the operator one.
            let path = explicit_socket.unwrap_or_else(|| agent_socket_path(&operator_path));
            let mut bridge = McpBridge::new(path, consent.into(), client_name);
            match bridge.serve_stdio() {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("mutiny mcp error: {err}");
                    ExitCode::from(1)
                }
            }
        }
        None => run_gui_app(
            operator_path,
            cli.working_directory,
            cli.shell,
            cli.agent_consent.into(),
            cli.no_restore,
            cli.http_bind.map(|bind| HttpOptions {
                bind,
                token: cli.http_token,
            }),
            Overrides {
                font_family: cli.font,
                font_size: cli.font_size,
                initial_cols: cli.cols,
                initial_rows: cli.rows,
            },
        ),
    }
}

fn run_ctl(socket_path: &Path, verb: CtlVerb) -> ExitCode {
    let timeout = match &verb {
        CtlVerb::Wait { timeout_ms, .. } | CtlVerb::AwaitTurn { timeout_ms, .. } => {
            Duration::from_millis(timeout_ms.saturating_add(1_500))
        }
        _ => Duration::from_secs(5),
    };

    let req = match verb {
        CtlVerb::List => IpcRequest::List,
        CtlVerb::Actions => IpcRequest::Actions,
        CtlVerb::Audit { limit } => IpcRequest::AuditLog { limit },
        CtlVerb::Snapshot { pane, max_lines } => IpcRequest::Snapshot {
            pane: pane.map(PaneId),
            max_lines,
        },
        CtlVerb::ReadExited { pane } => IpcRequest::ReadExited {
            pane: pane.map(PaneId),
        },
        CtlVerb::Search {
            pattern,
            pane,
            max_matches,
        } => IpcRequest::Search {
            pane: pane.map(PaneId),
            pattern,
            ignore_case: true,
            max_matches,
        },
        CtlVerb::Run { slug, window } => {
            let Some(spec) = by_slug(&slug) else {
                eprintln!(
                    "unknown action slug '{slug}'; run `mutiny ctl actions` to inspect valid slugs"
                );
                return ExitCode::from(2);
            };
            IpcRequest::RunAction {
                action: spec.id,
                window: window.map(WindowId),
            }
        }
        CtlVerb::SpawnWindow { cwd } => IpcRequest::SpawnWindow { cwd },
        CtlVerb::SpawnTab { window, cwd } => IpcRequest::SpawnTab {
            window: window.map(WindowId),
            cwd,
        },
        CtlVerb::Split { pane, down, cwd } => IpcRequest::Split {
            pane: pane.map(PaneId),
            axis: if down {
                SplitAxis::Vertical
            } else {
                SplitAxis::Horizontal
            },
            cwd,
        },
        CtlVerb::Focus { pane } => IpcRequest::Focus { pane: PaneId(pane) },
        CtlVerb::DetachSplit { pane } => IpcRequest::DetachSplit {
            pane: pane.map(PaneId),
        },
        CtlVerb::MovePane {
            pane,
            target_tab,
            down,
        } => IpcRequest::MovePane {
            pane: PaneId(pane),
            target_tab: TabId(target_tab),
            axis: if down {
                SplitAxis::Vertical
            } else {
                SplitAxis::Horizontal
            },
        },
        CtlVerb::SendText { text, pane } => IpcRequest::SendText {
            pane: pane.map(PaneId),
            text,
        },
        CtlVerb::SendInput {
            pane,
            text,
            keys,
            bracketed_paste,
            return_snapshot,
        } => IpcRequest::SendInput {
            pane: pane.map(PaneId),
            text,
            keys,
            bracketed_paste,
            return_snapshot,
        },
        CtlVerb::SendToAgent {
            message,
            pane,
            max_lines,
        } => IpcRequest::SendToAgent {
            pane: pane.map(PaneId),
            message,
            max_lines,
        },
        CtlVerb::PresentNotice { pane, message } => IpcRequest::PresentNotice {
            pane: pane.map(PaneId),
            message,
        },
        CtlVerb::PresentMedia {
            path,
            pane,
            cols,
            rows,
        } => IpcRequest::PresentMedia {
            pane: pane.map(PaneId),
            path,
            cols,
            rows,
        },
        CtlVerb::LastOutput { pane } => IpcRequest::LastOutput {
            pane: pane.map(PaneId),
        },
        CtlVerb::Screen { pane } => IpcRequest::ScreenText {
            pane: pane.map(PaneId),
        },
        CtlVerb::Wait {
            pane,
            since_seq,
            timeout_ms,
        } => IpcRequest::WaitPrompt {
            pane: pane.map(PaneId),
            since_seq,
            timeout_ms,
        },
        CtlVerb::AwaitTurn {
            pane,
            since_prompt_seq,
            since_output_seq,
            settle_ms,
            timeout_ms,
        } => IpcRequest::AwaitTurn {
            pane: pane.map(PaneId),
            since_prompt_seq,
            since_output_seq,
            settle_ms,
            timeout_ms,
        },
    };

    match send_request(socket_path, &req, timeout) {
        Ok(resp @ IpcResponse::Ok { .. }) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&resp).expect("serialize IPC response")
            );
            ExitCode::SUCCESS
        }
        Ok(IpcResponse::Error { message }) => {
            eprintln!("mutiny ctl error: {message}");
            ExitCode::from(1)
        }
        Err(err) => {
            eprintln!("failed to reach mutiny at {}: {err}", socket_path.display());
            ExitCode::from(1)
        }
    }
}

/// GUI-only settings for the authenticated HTTP MCP carrier. Grouped into one
/// argument so the listener stays optional without widening `run_gui_app`.
struct HttpOptions {
    bind: std::net::SocketAddr,
    token: Option<String>,
}

fn run_gui_app(
    socket_path: PathBuf,
    initial_cwd: Option<PathBuf>,
    shell_prog: Option<String>,
    agent_consent: ConsentMode,
    no_restore: bool,
    http: Option<HttpOptions>,
    overrides: Overrides,
) -> ExitCode {
    if socket_path.exists()
        && let Ok(IpcResponse::Ok { .. }) = send_request(
            &socket_path,
            &IpcRequest::SpawnWindow {
                cwd: initial_cwd.clone(),
            },
            Duration::from_secs(2),
        )
    {
        return ExitCode::SUCCESS;
    }

    let app = gtk4::Application::builder()
        .application_id("dev.mutiny.Terminal")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let shell_override = shell_prog.map(|p| (p, Vec::new()));
    // Precedence: CLI flag, then config.toml, then built-in default. Overrides
    // are re-applied on every live reload, so editing the file cannot discard
    // a flag the operator launched with.
    let controller = AppController::with_config(
        shell_override,
        default_audit_path(),
        config::config_path(),
        overrides,
    );

    if let Err(err) = controller.start_ipc_listener(&socket_path) {
        eprintln!(
            "warning: could not bind IPC socket at {}: {err}",
            socket_path.display()
        );
    }

    let agent_path = agent_socket_path(&socket_path);
    if let Err(err) = controller.start_agent_listener(&agent_path, agent_consent) {
        eprintln!(
            "warning: could not bind agent socket at {}: {err}",
            agent_path.display()
        );
    }

    // Restore the saved session before the first window is created, so
    // connect_activate only spawns a fresh pane when there is nothing to
    // restore. Processes are not restored; shells start fresh in saved cwds.
    if !no_restore {
        match controller.restore_session_from_file() {
            Ok(Some(n)) => eprintln!("mutiny: restored {n} panes from the saved session"),
            Ok(None) => {}
            Err(err) => eprintln!("warning: session not restored: {err}"),
        }
    }

    // Authenticated HTTP MCP carrier. Off unless explicitly bound, and it
    // forwards to the agent socket only, so a network peer can never be
    // assigned Operator.
    let mut http_handle = None;
    if let Some(http) = http {
        let token = match http.token {
            Some(token) => token,
            None => match http_mcp::generate_token() {
                Ok(token) => {
                    eprintln!("mutiny: HTTP MCP pairing token: {token}");
                    token
                }
                Err(err) => {
                    // An empty token is refused by spawn, so a failed CSPRNG
                    // read leaves the listener down rather than open.
                    eprintln!("warning: could not generate a pairing token: {err}");
                    String::new()
                }
            },
        };
        match http_mcp::spawn(HttpMcpConfig {
            bind: http.bind,
            token,
            agent_socket: agent_socket_path(&socket_path),
        }) {
            Ok(handle) => {
                eprintln!("mutiny: HTTP MCP listening on {}", handle.local_addr);
                http_handle = Some(handle);
            }
            Err(err) => eprintln!("warning: HTTP MCP listener not started: {err}"),
        }
    }

    let ctrl_startup = Rc::clone(&controller);
    app.connect_startup(move |gtk_app| {
        ctrl_startup.attach_gtk_app(gtk_app);
        gtk_app.set_menubar(Some(&build_menubar_model()));

        for spec in ACTIONS {
            if let Some(accel) = spec.default_accel {
                gtk_app.set_accels_for_action(&spec.gtk_detailed_name(), &[accel]);
            }
        }
    });

    let ctrl_activate = Rc::clone(&controller);
    app.connect_activate(move |_| {
        if ctrl_activate.workspace.borrow().windows().is_empty()
            && let Err(err) = ctrl_activate.create_window(initial_cwd.clone())
        {
            eprintln!("failed to spawn initial terminal window: {err}");
        }
    });

    let ctrl_tick = Rc::clone(&controller);
    glib::timeout_add_local(Duration::from_millis(4), move || {
        ctrl_tick.poll_once();
        glib::ControlFlow::Continue
    });

    let code = app.run_with_args::<&str>(&[]);
    if let Some(mut handle) = http_handle {
        handle.stop();
    }
    // Save on clean exit. A crash loses changes since the last save, which is
    // the accepted consequence of having no multiplexer daemon.
    if let Err(err) = controller.save_session() {
        eprintln!("warning: session not saved: {err}");
    }
    controller.shutdown_ipc();
    ExitCode::from(code.get())
}
