use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gtk4::gdk;
use gtk4::prelude::*;

use crate::action::ActionId;
use crate::config::{self, Overrides, RELOAD_CHECK_INTERVAL, ResolvedConfig};
use crate::engine::{
    EngineNotification, GridSize, PaneEngine, SEARCH_LINES_PER_TICK, ScrollbackSearch,
    parse_named_key,
};
use crate::ipc::{
    AuditDecision, AuditEntry, ConsentMode, ConsentOutcome, IpcEnvelope, IpcRequest, IpcResponse,
    PeerIdentity, action_catalog, bind_socket, carrier_permits_peer, consent_outcome,
    default_audit_path, peer_credentials, read_envelope, server_uid, write_response,
};
use crate::mux::{
    self, DISCOVERY_BYTES_PER_TICK, HostState, MAX_CONCURRENT_DISCOVERIES, MAX_DISCOVERY_BYTES,
    MuxHostStatus, MuxInventory, MuxKind, MuxSession,
};
use crate::notify::{DBusNotifier, DesktopNotifier};
use crate::session::{self, SessionState};
use crate::topology::{PaneId, SplitAxis, TabId, TopologyError, WindowId, Workspace};
use crate::ui::window::MutinyWindow;

const MAX_AUDIT_ENTRIES: usize = 256;
/// Rotate the durable trail past this size, keeping one previous generation.
const AUDIT_ROTATE_BYTES: u64 = 8 * 1024 * 1024;

struct PendingWait {
    pane: PaneId,
    since_prompt_seq: u64,
    since_output_seq: Option<u64>,
    settle: Duration,
    deadline: Instant,
    stream: UnixStream,
}

/// An IPC search parked across ticks. Holds its own cursor so it does not
/// disturb the viewport bar's search on the same pane. The response shape is
/// unchanged; only the latency differs.
struct PendingSearch {
    pane: PaneId,
    search: ScrollbackSearch,
    deadline: Instant,
    stream: UnixStream,
}

pub struct PendingConsent {
    pub id: u64,
    pub client: String,
    pub request: IpcRequest,
    pub carrier: ConsentMode,
    /// Kernel-verified peer, so an Allow Session grant binds to the process
    /// that asked rather than to the name it typed.
    pub peer: PeerIdentity,
    pub deadline: Instant,
    stream: UnixStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentResolution {
    AllowOnce,
    AllowSession,
    Deny,
}

/// Which session to open, grouped so the shared open path stays readable.
struct SessionRequest<'a> {
    kind: MuxKind,
    name: &'a str,
    host: Option<&'a str>,
}

/// A one-shot multiplexer command and what to do when it succeeds.
struct PendingMuxCommand {
    child: std::process::Child,
    out: Vec<u8>,
    err: Vec<u8>,
    deadline: Instant,
    killed: bool,
    effect: MuxEffect,
}

/// Applied only after the command exits successfully, so a failed rename does
/// not leave mutiny claiming a name the multiplexer never accepted.
enum MuxEffect {
    Rename {
        kind: MuxKind,
        host: Option<String>,
        old: String,
        new: String,
    },
}

/// A finished discovery job: which target, its sessions or an error, and how
/// long it took.
type DiscoveryOutcome = (
    Option<String>,
    MuxKind,
    Result<Vec<MuxSession>, String>,
    u64,
);

/// One in-flight discovery child. Spawned on the tick (fork+exec is well under
/// a millisecond) and polled with `try_wait`, so nothing slow ever blocks the
/// GUI loop. Threads are deliberately not used: alacritty's `tty::setup_env`
/// mutates the process environment on every pane spawn, so a worker thread
/// reading `environ` concurrently would be unsound.
struct PendingDiscovery {
    host: Option<String>,
    kind: MuxKind,
    child: std::process::Child,
    out: Vec<u8>,
    err: Vec<u8>,
    started: Instant,
    deadline: Instant,
    /// Set once the deadline passed and we sent SIGKILL. The job stays until
    /// `try_wait` reaps it, because `Child::drop` neither kills nor reaps.
    killed: bool,
}

/// One relay carrier for an HTTP peer token. The identity is assigned when the
/// operator configures the peer, not by anything the remote client sends.
struct HttpPeerCarrier {
    listener: UnixListener,
    path: PathBuf,
    identity: PeerIdentity,
    label: String,
    /// Mode this peer's carrier assigns. Pinned per peer at launch, so one peer
    /// can be paired while another is observe-only. Never `Operator`: the only
    /// way in is `ConsentMode::parse_peer_mode`, which refuses it.
    mode: ConsentMode,
}

pub struct AppController {
    pub workspace: RefCell<Workspace>,
    gtk_app: RefCell<Option<gtk4::Application>>,
    ui_windows: RefCell<HashMap<WindowId, MutinyWindow>>,
    ipc_listener: RefCell<Option<UnixListener>>,
    socket_path: RefCell<Option<PathBuf>>,
    agent_listener: RefCell<Option<UnixListener>>,
    agent_socket_path: RefCell<Option<PathBuf>>,
    /// One relay carrier per HTTP peer token, each assigning a distinct
    /// token-derived identity. Empty unless the HTTP listener is enabled.
    http_peers: RefCell<Vec<HttpPeerCarrier>>,
    discovery_jobs: RefCell<Vec<PendingDiscovery>>,
    discovery_queue: RefCell<VecDeque<(Option<String>, MuxKind)>>,
    session_cache: RefCell<MuxInventory>,
    /// Bumped whenever the cache changes, so the picker refreshes on change
    /// rather than every 4 ms tick.
    cache_gen: Cell<u64>,
    pending_session_reply: RefCell<Option<UnixStream>>,
    discovery_deadline: Cell<Option<Instant>>,
    /// Every live Sessions submenu (the app menubar and each window's own), so
    /// discovery results fill them all.
    sessions_menus: RefCell<Vec<gtk4::gio::Menu>>,
    sessions_menu_gen: Cell<u64>,
    /// Last agent-active state pushed to the window icons, so the icon is only
    /// touched on a transition.
    agent_icon_active: Cell<bool>,
    last_discovery_started: Cell<Option<Instant>>,
    /// In-flight one-shot multiplexer commands (rename). Same tick-polled shape
    /// as discovery: a rename may go over ssh, so it must not block the loop.
    mux_commands: RefCell<Vec<PendingMuxCommand>>,
    /// Mode the agent carrier assigns to every connection it accepts. Pinned
    /// by the operator at GUI launch; no client can raise it.
    agent_mode: Cell<ConsentMode>,
    audit_path: RefCell<Option<PathBuf>>,
    config: RefCell<ResolvedConfig>,
    config_path: PathBuf,
    config_overrides: Overrides,
    config_mtime: Cell<Option<SystemTime>>,
    last_config_check: Cell<Instant>,
    pending_waits: RefCell<Vec<PendingWait>>,
    pending_searches: RefCell<Vec<PendingSearch>>,
    pending_consents: RefCell<VecDeque<PendingConsent>>,
    session_approved_clients: RefCell<HashSet<PeerIdentity>>,
    /// Display labels for the granted identities, so the audit dialog can show
    /// a human name while the grant itself keys on the verified peer.
    session_grant_labels: RefCell<HashMap<PeerIdentity, String>>,
    audit_log: RefCell<VecDeque<AuditEntry>>,
    next_audit_seq: Cell<u64>,
    next_consent_id: Cell<u64>,
    recent_agent: RefCell<Option<(String, Instant)>>,
    shell_override: RefCell<Option<(String, Vec<String>)>>,
    default_menubar: Cell<bool>,
    notifier: RefCell<Rc<dyn DesktopNotifier>>,
    session_path: RefCell<PathBuf>,
}

impl AppController {
    pub fn new(shell_override: Option<(String, Vec<String>)>) -> Rc<Self> {
        Self::with_audit_path(shell_override, default_audit_path())
    }

    /// Builds a controller whose durable audit trail lives at `audit_path`.
    /// Existing entries are loaded so the trail survives a restart.
    pub fn with_audit_path(
        shell_override: Option<(String, Vec<String>)>,
        audit_path: PathBuf,
    ) -> Rc<Self> {
        Self::with_config(
            shell_override,
            audit_path,
            config::config_path(),
            Overrides::default(),
        )
    }

    /// Builds a controller with an explicit config path and CLI overrides.
    /// Precedence is flag, then file, then default; overrides are re-applied on
    /// every live reload so editing the file cannot discard a launch flag.
    pub fn with_config(
        shell_override: Option<(String, Vec<String>)>,
        audit_path: PathBuf,
        config_path: PathBuf,
        overrides: Overrides,
    ) -> Rc<Self> {
        let mut ring = VecDeque::with_capacity(MAX_AUDIT_ENTRIES);
        let next_seq = load_audit_tail(&audit_path, &mut ring);
        let loaded = match config::load_file(&config_path) {
            Ok(cfg) => cfg,
            Err(err) => {
                eprintln!("warning: config not loaded, using defaults: {err}");
                config::Config::default()
            }
        };
        let resolved = config::resolve(&loaded, &overrides);
        let mtime = config::file_mtime(&config_path);
        Rc::new(Self {
            workspace: RefCell::new(Workspace::new()),
            gtk_app: RefCell::new(None),
            ui_windows: RefCell::new(HashMap::new()),
            ipc_listener: RefCell::new(None),
            socket_path: RefCell::new(None),
            agent_listener: RefCell::new(None),
            http_peers: RefCell::new(Vec::new()),
            discovery_jobs: RefCell::new(Vec::new()),
            discovery_queue: RefCell::new(VecDeque::new()),
            session_cache: RefCell::new(MuxInventory::default()),
            cache_gen: Cell::new(0),
            pending_session_reply: RefCell::new(None),
            discovery_deadline: Cell::new(None),
            sessions_menus: RefCell::new(Vec::new()),
            sessions_menu_gen: Cell::new(u64::MAX),
            agent_icon_active: Cell::new(false),
            last_discovery_started: Cell::new(None),
            mux_commands: RefCell::new(Vec::new()),
            agent_socket_path: RefCell::new(None),
            agent_mode: Cell::new(ConsentMode::Ask),
            audit_path: RefCell::new(Some(audit_path)),
            config: RefCell::new(resolved),
            config_path,
            config_overrides: overrides,
            config_mtime: Cell::new(mtime),
            last_config_check: Cell::new(Instant::now()),
            pending_waits: RefCell::new(Vec::new()),
            pending_searches: RefCell::new(Vec::new()),
            pending_consents: RefCell::new(VecDeque::new()),
            session_approved_clients: RefCell::new(HashSet::new()),
            session_grant_labels: RefCell::new(HashMap::new()),
            audit_log: RefCell::new(ring),
            next_audit_seq: Cell::new(next_seq),
            next_consent_id: Cell::new(1),
            recent_agent: RefCell::new(None),
            shell_override: RefCell::new(shell_override),
            default_menubar: Cell::new(true),
            notifier: RefCell::new(Rc::new(DBusNotifier)),
            session_path: RefCell::new(session::session_path()),
        })
    }

    /// Pins the mode the agent carrier will assign. Call before accepting
    /// connections; this is the operator-side grant, not a client request.
    pub fn set_agent_mode(&self, mode: ConsentMode) {
        self.agent_mode.set(mode);
    }

    pub fn agent_mode(&self) -> ConsentMode {
        self.agent_mode.get()
    }

    /// Overrides the session state path. Tests point this at a temp file so
    /// they never touch the user's real state directory.
    pub fn set_session_path(&self, path: PathBuf) {
        *self.session_path.borrow_mut() = path;
    }

    /// Writes the current workspace to the session state file.
    pub fn save_session(&self) -> io::Result<()> {
        let state = session::capture(&self.workspace.borrow());
        session::save(&self.session_path.borrow(), &state)
    }

    /// Loads and restores the session file, if present. Returns the pane count
    /// restored, or None when there was no state file.
    pub fn restore_session_from_file(self: &Rc<Self>) -> Result<Option<usize>, String> {
        match session::load(&self.session_path.borrow())? {
            Some(state) => Ok(Some(self.restore_session(state)?)),
            None => Ok(None),
        }
    }

    /// Restores a saved bundle: window and tab order, split trees and ratios,
    /// working directories, and scrollback. Mutiny-owned shells start fresh;
    /// the no-daemon crash contract stands for them.
    ///
    /// A pane that was attached to a multiplexer re-attaches to that session
    /// instead. Those processes belong to the multiplexer, so this is not
    /// process resurrection by mutiny.
    pub fn restore_session(self: &Rc<Self>, state: SessionState) -> Result<usize, String> {
        let opts = self.config.borrow().sessions.clone();
        let mut engines = Vec::new();
        for sp in &state.panes {
            let attach = match &sp.mux {
                Some(session) => match &session.host {
                    None => Some(mux::attach_argv(session.kind, &session.name, &opts)),
                    Some(host) => Some(
                        mux::remote_attach_argv(host, session.kind, &session.name, &opts)
                            .map_err(|e| format!("pane {}: {e}", sp.id))?,
                    ),
                },
                None => None,
            };
            // A remote session's saved cwd is a path on that host, never a
            // local directory.
            let cwd = match (&sp.mux, attach.is_some()) {
                (Some(session), true) if session.host.is_some() => None,
                _ => sp.cwd.clone(),
            };
            let mut eng = self
                .spawn_engine(sp.id, cwd, attach.clone())
                .map_err(|e| format!("pane {}: {e}", sp.id))?;
            eng.resize(GridSize::new(sp.cols, sp.rows, 9, 18));
            eng.set_title(sp.title.clone());
            if let Some(session) = &sp.mux {
                eng.set_mux_session(Some(session.clone()));
                // The multiplexer owns the screen and will redraw it. Replaying
                // saved text into an attaching pane would corrupt that display.
            } else {
                eng.replay_scrollback(&sp.scrollback);
            }
            engines.push((sp.id, eng));
        }
        let count = engines.len();
        session::install(&mut self.workspace.borrow_mut(), state, engines);
        let ids: Vec<WindowId> = self
            .workspace
            .borrow()
            .windows()
            .iter()
            .map(|w| w.id)
            .collect();
        for id in ids {
            self.ensure_ui_window(id);
            self.sync_ui_window(id);
        }
        Ok(count)
    }

    /// Replaces the desktop notifier. Tests inject a recorder so forwarding
    /// can be asserted without a session bus.
    pub fn set_notifier(&self, notifier: Rc<dyn DesktopNotifier>) {
        *self.notifier.borrow_mut() = notifier;
    }

    /// Zero-copy read of the effective config, for the renderer.
    pub fn config_ref(&self) -> std::cell::Ref<'_, ResolvedConfig> {
        self.config.borrow()
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Stats the config file on a coarse interval and reloads when it changed.
    /// Returns true when a new config was applied. A parse failure keeps the
    /// previous config and does not retry until the file changes again, so a
    /// half-written file cannot spam warnings every tick.
    pub fn reload_config_if_changed(self: &Rc<Self>) -> bool {
        let now = Instant::now();
        if now.duration_since(self.last_config_check.get()) < RELOAD_CHECK_INTERVAL {
            return false;
        }
        self.last_config_check.set(now);

        let mtime = config::file_mtime(&self.config_path);
        if mtime == self.config_mtime.get() {
            return false;
        }
        self.config_mtime.set(mtime);

        let loaded = match config::load_file(&self.config_path) {
            Ok(cfg) => cfg,
            Err(err) => {
                eprintln!("warning: config reload failed, keeping previous: {err}");
                return false;
            }
        };
        let resolved = config::resolve(&loaded, &self.config_overrides);
        if *self.config.borrow() == resolved {
            return false;
        }
        *self.config.borrow_mut() = resolved;
        self.apply_config_to_views();
        true
    }

    fn apply_config_to_views(self: &Rc<Self>) {
        let (bg, fg) = {
            let cfg = self.config.borrow();
            (cfg.background, cfg.foreground)
        };
        // Collect first: a temporary borrow in the `for` expression lives for
        // the whole loop, so borrowing mutably inside it would panic.
        let pane_ids = self.workspace.borrow().pane_ids();
        for pid in pane_ids {
            if let Some(eng) = self.workspace.borrow_mut().pane_mut(pid) {
                eng.set_reported_colors(bg, fg);
            }
        }
        let wins: Vec<MutinyWindow> = self.ui_windows.borrow().values().cloned().collect();
        for w in wins {
            w.refresh_from_config();
        }
        for pid in self.workspace.borrow().pane_ids() {
            self.queue_redraw_pane(pid);
        }
    }

    pub fn attach_gtk_app(self: &Rc<Self>, app: &gtk4::Application) {
        *self.gtk_app.borrow_mut() = Some(app.clone());
        // Any model window that exists without a view gets one now. A restore
        // that ran before the application was attached populated the model but
        // could not create widgets, leaving an app with nothing to show, which
        // then exited immediately. Doing it here makes the order safe either
        // way rather than relying on the caller's sequencing.
        let ids: Vec<WindowId> = self
            .workspace
            .borrow()
            .windows()
            .iter()
            .map(|w| w.id)
            .collect();
        for id in ids {
            self.ensure_ui_window(id);
            self.sync_ui_window(id);
        }
    }

    pub fn start_ipc_listener(&self, path: &Path) -> io::Result<()> {
        let listener = bind_socket(path)?;
        *self.ipc_listener.borrow_mut() = Some(listener);
        *self.socket_path.borrow_mut() = Some(path.to_path_buf());
        Ok(())
    }

    /// Binds a relay carrier for one HTTP peer token, assigning the identity
    /// that token maps to. The HTTP carrier connects here after verifying the
    /// token, so the identity is fixed by operator configuration rather than
    /// anything the remote client writes. Grants and audit key on it, which is
    /// what makes two peers distinguishable.
    pub fn start_http_peer_listener(
        &self,
        path: &Path,
        token: &str,
        label: &str,
        mode: ConsentMode,
    ) -> io::Result<()> {
        // Defensive: the parse boundary already refuses Operator, and a carrier
        // that granted it over the network would break the whole model.
        if mode == ConsentMode::Operator {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "a network peer carrier can never assign Operator",
            ));
        }
        let listener = bind_socket(path)?;
        self.http_peers.borrow_mut().push(HttpPeerCarrier {
            listener,
            path: path.to_path_buf(),
            identity: PeerIdentity::for_token(token, server_uid()),
            label: label.to_owned(),
            mode,
        });
        Ok(())
    }

    /// Binds the agent carrier and pins the mode it assigns. Separate from the
    /// operator socket so an agent transport can never arrive on the carrier
    /// that grants `Operator`.
    pub fn start_agent_listener(&self, path: &Path, mode: ConsentMode) -> io::Result<()> {
        let listener = bind_socket(path)?;
        *self.agent_listener.borrow_mut() = Some(listener);
        *self.agent_socket_path.borrow_mut() = Some(path.to_path_buf());
        self.agent_mode.set(mode);
        Ok(())
    }

    pub fn shutdown_ipc(&self) {
        *self.ipc_listener.borrow_mut() = None;
        if let Some(path) = self.socket_path.borrow_mut().take() {
            let _ = std::fs::remove_file(path);
        }
        *self.agent_listener.borrow_mut() = None;
        if let Some(path) = self.agent_socket_path.borrow_mut().take() {
            let _ = std::fs::remove_file(path);
        }
        for peer in self.http_peers.borrow_mut().drain(..) {
            let _ = std::fs::remove_file(peer.path);
        }
        self.shutdown_discoveries();
    }

    pub fn active_agent_badge(&self) -> Option<String> {
        let guard = self.recent_agent.borrow();
        let (name, ts) = guard.as_ref()?;
        if ts.elapsed() <= Duration::from_secs(15) {
            Some(name.clone())
        } else {
            None
        }
    }

    pub fn pending_consent_prompt(&self) -> Option<(u64, String, String, String)> {
        let guard = self.pending_consents.borrow();
        let front = guard.front()?;
        Some((
            front.id,
            front.client.clone(),
            front.request.verb_name().to_owned(),
            front.request.redacted_summary(),
        ))
    }

    /// Labels of the granted peer identities, for the audit dialog. The grant
    /// itself keys on `PeerIdentity`; this is display text only.
    pub fn session_grants(&self) -> Vec<String> {
        let grants = self.session_approved_clients.borrow();
        let labels = self.session_grant_labels.borrow();
        let mut out: Vec<String> = grants
            .iter()
            .map(|id| {
                let label = labels
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| String::from("unknown"));
                format!("{label} (pid {})", id.pid)
            })
            .collect();
        out.sort();
        out
    }

    /// Grants a session to a kernel-verified peer. This is the operator-side
    /// action the consent bar's Allow Session button performs; it is not
    /// reachable from the IPC surface, so a client cannot grant itself.
    pub fn grant_session(&self, peer: PeerIdentity, label: &str) {
        self.session_approved_clients.borrow_mut().insert(peer);
        self.session_grant_labels
            .borrow_mut()
            .insert(peer, label.to_owned());
        self.refresh_all_window_chromes();
    }

    pub fn revoke_session_grants(&self) {
        self.session_approved_clients.borrow_mut().clear();
        self.session_grant_labels.borrow_mut().clear();
        self.refresh_all_window_chromes();
    }

    pub fn audit_entries(&self, limit: usize) -> Vec<AuditEntry> {
        self.audit_log
            .borrow()
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect()
    }

    fn record_audit(
        &self,
        client: &str,
        req: &IpcRequest,
        decision: AuditDecision,
        carrier: ConsentMode,
        peer: PeerIdentity,
    ) {
        let seq = self.next_audit_seq.get();
        self.next_audit_seq.set(seq + 1);
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let entry = AuditEntry {
            seq,
            unix_ms,
            client: client.to_owned(),
            verb: req.verb_name().to_owned(),
            tier: req.consent_tier(),
            carrier,
            peer,
            decision,
            detail: req.redacted_summary(),
        };
        {
            let mut log = self.audit_log.borrow_mut();
            if log.len() >= MAX_AUDIT_ENTRIES {
                log.pop_front();
            }
            log.push_back(entry.clone());
        }
        if let Some(path) = self.audit_path.borrow().as_ref() {
            append_audit_entry(path, &entry);
        }
    }

    pub fn resolve_consent(self: &Rc<Self>, id: u64, resolution: ConsentResolution) {
        let maybe_item = {
            let mut queue = self.pending_consents.borrow_mut();
            queue
                .iter()
                .position(|c| c.id == id)
                .and_then(|idx| queue.remove(idx))
        };
        let Some(mut item) = maybe_item else {
            return;
        };

        match resolution {
            ConsentResolution::Deny => {
                self.record_audit(
                    &item.client,
                    &item.request,
                    AuditDecision::Denied,
                    item.carrier,
                    item.peer,
                );
                let _ = write_response(
                    &mut item.stream,
                    &IpcResponse::err("operator denied agent action"),
                );
            }
            ConsentResolution::AllowOnce => {
                self.record_audit(
                    &item.client,
                    &item.request,
                    AuditDecision::ApprovedOnce,
                    item.carrier,
                    item.peer,
                );
                if let Some(resp) = self.execute_ipc_request(item.request, &item.stream) {
                    let _ = write_response(&mut item.stream, &resp);
                }
            }
            ConsentResolution::AllowSession => {
                // Bind to the verified peer, not to the label it sent.
                self.grant_session(item.peer, &item.client);
                self.record_audit(
                    &item.client,
                    &item.request,
                    AuditDecision::ApprovedSession,
                    item.carrier,
                    item.peer,
                );
                if let Some(resp) = self.execute_ipc_request(item.request, &item.stream) {
                    let _ = write_response(&mut item.stream, &resp);
                }
            }
        }
        self.refresh_all_window_chromes();
    }

    /// `command` overrides the controller-wide shell for this one pane, which is
    /// how a multiplexer attach runs `tmux`/`ssh` instead of a login shell.
    fn spawn_engine(
        &self,
        pane_id: PaneId,
        cwd: Option<PathBuf>,
        command: Option<(String, Vec<String>)>,
    ) -> io::Result<PaneEngine> {
        let (cols, rows, scrollback, bg, fg) = {
            let cfg = self.config.borrow();
            (
                cfg.initial_cols,
                cfg.initial_rows,
                cfg.scrollback_lines,
                cfg.background,
                cfg.foreground,
            )
        };
        let size = GridSize::new(cols, rows, 9, 18);
        let shell = command.or_else(|| self.shell_override.borrow().clone());
        let mut engine = PaneEngine::spawn_pty(size, cwd, shell, pane_id.0, scrollback)?;
        // OSC 10/11 queries must answer with the colors actually rendered.
        engine.set_reported_colors(bg, fg);
        Ok(engine)
    }

    /// Attaches a pane to an existing multiplexer session, locally or over SSH.
    /// The multiplexer owns process survival; closing the pane kills only the
    /// client, never the server.
    pub fn open_session(
        self: &Rc<Self>,
        kind: MuxKind,
        name: &str,
        host: Option<&str>,
        mode: mux::SessionMode,
        window: Option<WindowId>,
        pane: Option<PaneId>,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        if name.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session name must not be empty",
            ));
        }
        self.attach_or_create(
            SessionRequest { kind, name, host },
            mode,
            window,
            pane,
            false,
        )
    }

    /// Creates a NEW session and opens a pane on it. Unlike `open_session` this
    /// must fail on a name collision rather than attaching to whatever already
    /// has that name, so it uses the create argv (no tmux `-A`).
    pub fn new_session(
        self: &Rc<Self>,
        kind: Option<MuxKind>,
        name: Option<String>,
        mode: mux::SessionMode,
        window: Option<WindowId>,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        let Some(kind) = kind else {
            // No multiplexer: a plain shell, which is what `preferred = "none"`
            // and the dialog's "none" choice mean.
            let cwd = self.active_cwd(window, None);
            return match mode {
                mux::SessionMode::Window => self.create_window(cwd),
                mux::SessionMode::Tab => self.create_tab(window, cwd),
                mux::SessionMode::SplitRight => self.split_pane(None, SplitAxis::Horizontal, cwd),
                mux::SessionMode::SplitDown => self.split_pane(None, SplitAxis::Vertical, cwd),
            };
        };
        let name = match name {
            Some(n) if !n.trim().is_empty() => n,
            _ => self.suggested_session_name(),
        };
        self.attach_or_create(
            SessionRequest {
                kind,
                name: &name,
                host: None,
            },
            mode,
            window,
            None,
            true,
        )
    }

    /// Renames a multiplexer session using its own command, then updates every
    /// pane attached to it. Returns once the command is launched; the rename is
    /// applied when it exits, because a remote rename can take seconds.
    pub fn rename_session(
        &self,
        kind: MuxKind,
        old: &str,
        new: &str,
        host: Option<&str>,
    ) -> io::Result<()> {
        if new.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the new session name must not be empty",
            ));
        }
        if old == new {
            return Ok(());
        }
        let opts = self.config.borrow().sessions.clone();
        let (program, argv) = mux::rename_command(kind, old, new, host, &opts)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let child = Command::new(&program)
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                io::Error::new(e.kind(), format!("could not run {program} to rename: {e}"))
            })?;
        if let Some(out) = child.stdout.as_ref() {
            set_nonblocking(out.as_raw_fd());
        }
        if let Some(err) = child.stderr.as_ref() {
            set_nonblocking(err.as_raw_fd());
        }
        let timeout = Duration::from_millis(self.config.borrow().discovery_timeout_ms);
        self.mux_commands.borrow_mut().push(PendingMuxCommand {
            child,
            out: Vec::new(),
            err: Vec::new(),
            deadline: Instant::now() + timeout,
            killed: false,
            effect: MuxEffect::Rename {
                kind,
                host: host.map(str::to_owned),
                old: old.to_owned(),
                new: new.to_owned(),
            },
        });
        Ok(())
    }

    /// Polls one-shot commands. Same nonblocking drain and kill-then-reap rules
    /// as discovery: `Child::drop` does neither.
    fn advance_mux_commands(self: &Rc<Self>) {
        if self.mux_commands.borrow().is_empty() {
            return;
        }
        let mut budget = DISCOVERY_BYTES_PER_TICK;
        let mut done: Vec<(MuxEffect, bool, String)> = Vec::new();
        {
            let mut jobs = self.mux_commands.borrow_mut();
            let mut i = 0;
            while i < jobs.len() {
                let job = &mut jobs[i];
                if let Some(out) = job.child.stdout.as_mut() {
                    drain_bounded(out, &mut job.out, &mut budget);
                }
                if let Some(err) = job.child.stderr.as_mut() {
                    drain_bounded(err, &mut job.err, &mut budget);
                }
                if !job.killed && Instant::now() >= job.deadline {
                    let _ = job.child.kill();
                    job.killed = true;
                }
                match job.child.try_wait() {
                    Ok(Some(status)) => {
                        let ok = !job.killed && status.success();
                        let detail = if job.killed {
                            String::from("timed out")
                        } else {
                            String::from_utf8_lossy(&job.err).trim().to_owned()
                        };
                        done.push((
                            std::mem::replace(
                                &mut job.effect,
                                MuxEffect::Rename {
                                    kind: MuxKind::Tmux,
                                    host: None,
                                    old: String::new(),
                                    new: String::new(),
                                },
                            ),
                            ok,
                            detail,
                        ));
                        jobs.remove(i);
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        done.push((
                            std::mem::replace(
                                &mut job.effect,
                                MuxEffect::Rename {
                                    kind: MuxKind::Tmux,
                                    host: None,
                                    old: String::new(),
                                    new: String::new(),
                                },
                            ),
                            false,
                            e.to_string(),
                        ));
                        jobs.remove(i);
                        continue;
                    }
                }
                i += 1;
            }
        }
        let mut changed = false;
        for (effect, ok, detail) in done {
            match effect {
                MuxEffect::Rename {
                    kind,
                    host,
                    old,
                    new,
                } => {
                    if ok {
                        self.apply_rename(kind, host.as_deref(), &old, &new);
                        changed = true;
                    } else if !new.is_empty() {
                        eprintln!("warning: rename {old:?} to {new:?} failed: {detail}");
                    }
                }
            }
        }
        if changed {
            self.refresh_all_window_chromes();
            let targets = self.discovery_targets_all();
            self.begin_discovery(targets, true, None);
        }
    }

    /// Updates every pane attached to the renamed session, so labels, window
    /// titles, and the saved bundle all follow the multiplexer's new name.
    fn apply_rename(&self, kind: MuxKind, host: Option<&str>, old: &str, new: &str) {
        let mut ws = self.workspace.borrow_mut();
        for pid in ws.pane_ids() {
            if let Some(eng) = ws.pane_mut(pid) {
                let matches = eng
                    .mux_session()
                    .is_some_and(|s| s.kind == kind && s.host.as_deref() == host && s.name == old);
                if matches {
                    eng.set_mux_session(Some(mux::MuxSessionRef {
                        kind,
                        name: mux::sanitize_text(new, 64),
                        host: host.map(|h| mux::sanitize_text(h, 64)),
                    }));
                }
            }
        }
    }

    /// Smallest free `mutiny-N` from the last discovery pass.
    pub fn suggested_session_name(&self) -> String {
        let taken: Vec<String> = self
            .session_cache
            .borrow()
            .sessions
            .iter()
            .map(|s| s.name.clone())
            .collect();
        mux::free_session_name(&taken)
    }

    fn attach_or_create(
        self: &Rc<Self>,
        request: SessionRequest<'_>,
        mode: mux::SessionMode,
        window: Option<WindowId>,
        pane: Option<PaneId>,
        create: bool,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        let SessionRequest { kind, name, host } = request;
        if name.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session name must not be empty",
            ));
        }
        let opts = self.config.borrow().sessions.clone();
        let command = match (host, create) {
            (None, false) => mux::attach_argv(kind, name, &opts),
            (None, true) => mux::create_argv(kind, name, &opts),
            (Some(host), false) => mux::remote_attach_argv(host, kind, name, &opts)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
            (Some(host), true) => mux::remote_create_argv(host, kind, name, &opts)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
        };
        // A remote session's path is a path on that host, so it must never
        // become a local cwd. alacritty ignores an invalid chdir silently.
        let cwd = if host.is_some() {
            None
        } else {
            self.active_cwd(window, pane)
        };
        let ids = match mode {
            mux::SessionMode::Window => self.create_window_with_command(cwd, Some(command))?,
            mux::SessionMode::Tab => self.create_tab_with_command(window, cwd, Some(command))?,
            mux::SessionMode::SplitRight => {
                self.split_pane_with_command(pane, SplitAxis::Horizontal, cwd, Some(command))?
            }
            mux::SessionMode::SplitDown => {
                self.split_pane_with_command(pane, SplitAxis::Vertical, cwd, Some(command))?
            }
        };
        if let Some(engine) = self.workspace.borrow_mut().pane_mut(ids.2) {
            engine.set_mux_session(Some(mux::MuxSessionRef {
                kind,
                name: mux::sanitize_text(name, 64),
                host: host.map(|h| mux::sanitize_text(h, 64)),
            }));
        }
        self.sync_ui_window(ids.0);
        Ok(ids)
    }

    /// Local plus every configured remote host. For the GUI picker, where the
    /// human is the consent. The IPC verb never uses this: `host: None` there
    /// means local only, so an Observe call cannot spend the user's credentials.
    pub fn discovery_targets_all(&self) -> Vec<Option<String>> {
        let mut targets = vec![None];
        let hosts = self.config.borrow().session_hosts.clone();
        let hosts = if hosts.is_empty() {
            std::fs::read_to_string(ssh_config_path())
                .map(|text| mux::ssh_config_hosts(&text))
                .unwrap_or_default()
        } else {
            hosts
        };
        for host in hosts.into_iter().take(mux::MAX_HOSTS_PER_REQUEST) {
            targets.push(Some(host));
        }
        targets
    }

    /// Expires the agent freshness window immediately. The window is 15 s in
    /// production, which is too long for a test to wait out.
    pub fn clear_recent_agent_for_test(&self) {
        *self.recent_agent.borrow_mut() = None;
    }

    /// Re-runs discovery on a modest cadence so the Sessions submenu reflects
    /// sessions that appeared or died since startup. This polls an external
    /// listing command; it owns no sessions and starts no daemon, so the
    /// crash-semantics contract is untouched.
    fn refresh_sessions_periodically(&self) {
        if self.gtk_app.borrow().is_none() || self.ui_windows.borrow().is_empty() {
            return;
        }
        let interval = self.config.borrow().session_refresh_interval_s;
        if interval == 0 || self.discovery_in_flight() {
            return;
        }
        let due = match self.last_discovery_started.get() {
            None => true,
            Some(at) => at.elapsed() >= Duration::from_secs(u64::from(interval)),
        };
        if due {
            let targets = self.discovery_targets_all();
            self.begin_discovery(targets, true, None);
        }
    }

    /// True while an external agent is driving the terminal. Derived from the
    /// same signal as the in-window agent badge, so the icon and the badge can
    /// never disagree: a non-Operator carrier call within the last 15 seconds.
    /// That window is also the debounce, so a brief act cannot flicker.
    pub fn agent_active(&self) -> bool {
        self.active_agent_badge().is_some()
    }

    /// Pushes the agent-active icon state to the default and every window, but
    /// only on a transition.
    fn refresh_agent_icon(&self) {
        // poll_once also runs headlessly, where GTK is not initialised and any
        // GTK call panics. No application means no windows to iconify.
        if self.gtk_app.borrow().is_none() {
            return;
        }
        let active = self.agent_active();
        if self.agent_icon_active.get() == active {
            return;
        }
        self.agent_icon_active.set(active);
        let name = if active {
            crate::desktop::AGENT_ICON_NAME
        } else {
            crate::desktop::APP_ID
        };
        gtk4::Window::set_default_icon_name(name);
        let wins: Vec<MutinyWindow> = self.ui_windows.borrow().values().cloned().collect();
        for w in wins {
            w.set_agent_active(active);
        }
    }

    /// Registers a Sessions submenu to be filled from discovery results.
    pub fn register_sessions_menu(&self, menu: gtk4::gio::Menu) {
        crate::action::populate_sessions_menu(&menu, &self.session_inventory());
        self.sessions_menus.borrow_mut().push(menu);
    }

    pub fn session_inventory(&self) -> MuxInventory {
        self.session_cache.borrow().clone()
    }

    pub fn session_cache_gen(&self) -> u64 {
        self.cache_gen.get()
    }

    pub fn discovery_in_flight(&self) -> bool {
        !self.discovery_jobs.borrow().is_empty() || !self.discovery_queue.borrow().is_empty()
    }

    /// Starts a discovery pass. Returns `Some` when the answer is already known
    /// (cached, or a pass is running and this caller gets the current snapshot),
    /// and `None` when the stream is parked until the pass completes.
    pub fn begin_discovery(
        &self,
        targets: Vec<Option<String>>,
        refresh: bool,
        stream: Option<UnixStream>,
    ) -> Option<IpcResponse> {
        if !refresh && !self.discovery_in_flight() && !self.session_cache.borrow().hosts.is_empty()
        {
            return Some(self.inventory_response());
        }
        // A second requester while a pass runs gets the snapshot rather than a
        // second parked stream: one reply slot, bounded.
        if self.discovery_in_flight() {
            if let Some(mut stream) = stream {
                let _ = write_response(&mut stream, &self.inventory_response());
            }
            return None;
        }

        let timeout = Duration::from_millis(self.config.borrow().discovery_timeout_ms);
        {
            let mut cache = self.session_cache.borrow_mut();
            // Keep the previous sessions until each target's result replaces
            // its slice. Clearing here would make the Sessions submenu flash
            // "No sessions found" at the start of every refresh.
            cache.hosts.clear();
            cache.partial = false;
            cache.scanned_at_ms = now_ms();
            for target in &targets {
                for kind in [MuxKind::Tmux, MuxKind::Zellij] {
                    cache.hosts.push(MuxHostStatus {
                        host: target.clone(),
                        kind,
                        state: HostState::Scanning,
                        error: None,
                        elapsed_ms: 0,
                    });
                }
            }
        }
        self.bump_cache_gen();
        {
            let mut queue = self.discovery_queue.borrow_mut();
            queue.clear();
            for target in targets {
                for kind in [MuxKind::Tmux, MuxKind::Zellij] {
                    queue.push_back((target.clone(), kind));
                }
            }
        }
        *self.pending_session_reply.borrow_mut() = stream;
        self.last_discovery_started.set(Some(Instant::now()));
        self.discovery_deadline.set(Some(Instant::now() + timeout));
        self.start_queued_discoveries();
        // Every program may have failed to spawn synchronously (not installed),
        // in which case the pass is already over and no tick will complete it.
        self.complete_discovery_pass();
        None
    }

    /// Writes the parked reply once no job or queued target remains. Called both
    /// from the tick and right after starting a pass, because a pass whose every
    /// spawn failed finishes before any tick runs.
    fn complete_discovery_pass(&self) {
        if !self.discovery_jobs.borrow().is_empty() || !self.discovery_queue.borrow().is_empty() {
            return;
        }
        self.discovery_deadline.set(None);
        if let Some(mut stream) = self.pending_session_reply.borrow_mut().take() {
            let _ = write_response(&mut stream, &self.inventory_response());
        }
    }

    fn inventory_response(&self) -> IpcResponse {
        let mut resp = IpcResponse::empty_ok();
        if let IpcResponse::Ok { sessions, .. } = &mut resp {
            *sessions = Some(self.session_inventory());
        }
        resp
    }

    fn bump_cache_gen(&self) {
        self.cache_gen.set(self.cache_gen.get().wrapping_add(1));
    }

    fn start_queued_discoveries(&self) {
        loop {
            if self.discovery_jobs.borrow().len() >= MAX_CONCURRENT_DISCOVERIES {
                break;
            }
            let next = self.discovery_queue.borrow_mut().pop_front();
            let Some((host, kind)) = next else { break };
            match self.spawn_discovery(host.clone(), kind) {
                Ok(job) => self.discovery_jobs.borrow_mut().push(job),
                Err(err) => self.record_discovery_result(host, kind, Err(err), 0),
            }
        }
    }

    fn spawn_discovery(
        &self,
        host: Option<String>,
        kind: MuxKind,
    ) -> Result<PendingDiscovery, String> {
        let opts = self.config.borrow().sessions.clone();
        let (program, argv) = match &host {
            None => (
                mux::local_program(kind, &opts),
                mux::discovery_argv(kind, &opts),
            ),
            Some(host) => mux::remote_discovery_argv(host, kind, &opts)?,
        };
        let child = Command::new(&program)
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    format!("{}: not installed ({program})", kind.label())
                } else {
                    format!("{}: could not run {program}: {e}", kind.label())
                }
            })?;
        if let Some(out) = child.stdout.as_ref() {
            set_nonblocking(out.as_raw_fd());
        }
        if let Some(err) = child.stderr.as_ref() {
            set_nonblocking(err.as_raw_fd());
        }
        let started = Instant::now();
        let timeout = Duration::from_millis(self.config.borrow().discovery_timeout_ms);
        Ok(PendingDiscovery {
            host,
            kind,
            child,
            out: Vec::new(),
            err: Vec::new(),
            started,
            deadline: started + timeout,
            killed: false,
        })
    }

    /// Polls discovery children without blocking. Called from `poll_once`.
    fn advance_discoveries(self: &Rc<Self>) {
        if self.discovery_jobs.borrow().is_empty() && self.discovery_queue.borrow().is_empty() {
            self.complete_discovery_pass();
            return;
        }
        let mut budget = DISCOVERY_BYTES_PER_TICK;
        let mut finished: Vec<DiscoveryOutcome> = Vec::new();
        {
            let mut jobs = self.discovery_jobs.borrow_mut();
            let mut index = 0;
            while index < jobs.len() {
                let job = &mut jobs[index];
                if let Some(out) = job.child.stdout.as_mut() {
                    drain_bounded(out, &mut job.out, &mut budget);
                }
                if let Some(err) = job.child.stderr.as_mut() {
                    drain_bounded(err, &mut job.err, &mut budget);
                }
                if !job.killed && Instant::now() >= job.deadline {
                    let _ = job.child.kill();
                    job.killed = true;
                }
                match job.child.try_wait() {
                    Ok(Some(status)) => {
                        let elapsed = job.started.elapsed().as_millis() as u64;
                        let stdout = String::from_utf8_lossy(&job.out).into_owned();
                        let stderr = String::from_utf8_lossy(&job.err).into_owned();
                        let result = if job.killed {
                            Err(format!("{}: timed out", job.kind.label()))
                        } else {
                            parse_discovery(
                                job.kind,
                                &stdout,
                                &stderr,
                                status.code(),
                                job.host.as_deref(),
                            )
                        };
                        finished.push((job.host.clone(), job.kind, result, elapsed));
                        jobs.remove(index);
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let elapsed = job.started.elapsed().as_millis() as u64;
                        finished.push((
                            job.host.clone(),
                            job.kind,
                            Err(format!("{}: {e}", job.kind.label())),
                            elapsed,
                        ));
                        jobs.remove(index);
                        continue;
                    }
                }
                index += 1;
            }
        }
        for (host, kind, result, elapsed) in finished {
            self.record_discovery_result(host, kind, result, elapsed);
        }
        self.start_queued_discoveries();

        self.complete_discovery_pass();
        self.refresh_session_pickers();
    }

    fn record_discovery_result(
        &self,
        host: Option<String>,
        kind: MuxKind,
        result: Result<Vec<MuxSession>, String>,
        elapsed_ms: u64,
    ) {
        let show_exited = self.config.borrow().show_exited_sessions;
        {
            let mut cache = self.session_cache.borrow_mut();
            cache
                .sessions
                .retain(|s| !(s.host == host && s.kind == kind));
            cache.hosts.retain(|h| !(h.host == host && h.kind == kind));
            let (state, error) = match &result {
                Ok(sessions) => {
                    for session in sessions {
                        if session.exited && !show_exited {
                            continue;
                        }
                        cache.sessions.push(session.clone());
                    }
                    (HostState::Ok, None)
                }
                Err(err) => (HostState::Error, Some(err.clone())),
            };
            cache.hosts.push(MuxHostStatus {
                host,
                kind,
                state,
                error,
                elapsed_ms,
            });
            if cache.sessions.len() > mux::MAX_SESSIONS_PER_HOST * mux::MAX_HOSTS_PER_REQUEST {
                cache
                    .sessions
                    .truncate(mux::MAX_SESSIONS_PER_HOST * mux::MAX_HOSTS_PER_REQUEST);
            }
        }
        self.bump_cache_gen();
    }

    /// Kills and reaps any outstanding discovery child. `Child::drop` does
    /// neither, so leaving them would leak zombies.
    fn shutdown_discoveries(&self) {
        self.discovery_queue.borrow_mut().clear();
        let jobs = std::mem::take(&mut *self.discovery_jobs.borrow_mut());
        for mut job in jobs {
            let _ = job.child.kill();
            let _ = job.child.wait();
        }
        self.pending_session_reply.borrow_mut().take();
        self.discovery_deadline.set(None);
    }

    fn active_cwd(&self, win_hint: Option<WindowId>, pane_hint: Option<PaneId>) -> Option<PathBuf> {
        let ws = self.workspace.borrow();
        let pid = pane_hint
            .or_else(|| win_hint.and_then(|w| ws.window(w)?.active_pane()))
            .or_else(|| ws.active_pane_id())?;
        ws.pane(pid)?.cwd().map(Path::to_path_buf)
    }

    pub fn create_window(
        self: &Rc<Self>,
        cwd: Option<PathBuf>,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        self.create_window_with_command(cwd, None)
    }

    /// New window whose pane runs `command` instead of a login shell.
    pub fn create_window_with_command(
        self: &Rc<Self>,
        cwd: Option<PathBuf>,
        command: Option<(String, Vec<String>)>,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        let inherited_cwd = cwd.or_else(|| self.active_cwd(None, None));
        let pane_id = self.workspace.borrow_mut().reserve_pane_id();
        let engine = self.spawn_engine(pane_id, inherited_cwd, command)?;
        let (win_id, tab_id) = self.workspace.borrow_mut().insert_window_with_pane(
            pane_id,
            engine,
            self.default_menubar.get(),
        );
        self.ensure_ui_window(win_id);
        self.sync_ui_window(win_id);
        Ok((win_id, tab_id, pane_id))
    }

    pub fn create_tab(
        self: &Rc<Self>,
        target_win: Option<WindowId>,
        cwd: Option<PathBuf>,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        self.create_tab_with_command(target_win, cwd, None)
    }

    /// New tab whose pane runs `command` instead of a login shell.
    pub fn create_tab_with_command(
        self: &Rc<Self>,
        target_win: Option<WindowId>,
        cwd: Option<PathBuf>,
        command: Option<(String, Vec<String>)>,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        let win_id = target_win
            .or_else(|| self.workspace.borrow().active_window_id())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no active window"))?;
        let inherited_cwd = cwd.or_else(|| self.active_cwd(Some(win_id), None));
        let pane_id = self.workspace.borrow_mut().reserve_pane_id();
        let engine = self.spawn_engine(pane_id, inherited_cwd, command)?;
        let tab_id = self
            .workspace
            .borrow_mut()
            .insert_tab_with_pane(win_id, pane_id, engine)
            .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e.to_string()))?;
        self.sync_ui_window(win_id);
        Ok((win_id, tab_id, pane_id))
    }

    pub fn split_pane(
        self: &Rc<Self>,
        target_pane: Option<PaneId>,
        axis: SplitAxis,
        cwd: Option<PathBuf>,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        self.split_pane_with_command(target_pane, axis, cwd, None)
    }

    /// New split whose pane runs `command` instead of a login shell.
    pub fn split_pane_with_command(
        self: &Rc<Self>,
        target_pane: Option<PaneId>,
        axis: SplitAxis,
        cwd: Option<PathBuf>,
        command: Option<(String, Vec<String>)>,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        let anchor = target_pane
            .or_else(|| self.workspace.borrow().active_pane_id())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no active pane"))?;
        let inherited_cwd = cwd.or_else(|| self.active_cwd(None, Some(anchor)));
        let new_pane = self.workspace.borrow_mut().reserve_pane_id();
        let engine = self.spawn_engine(new_pane, inherited_cwd, command)?;
        let (win_id, tab_id) = self
            .workspace
            .borrow_mut()
            .split_pane_with(anchor, axis, new_pane, engine)
            .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e.to_string()))?;
        self.sync_ui_window(win_id);
        Ok((win_id, tab_id, new_pane))
    }

    pub fn detach_split(
        self: &Rc<Self>,
        target_pane: Option<PaneId>,
    ) -> Result<(WindowId, TabId, PaneId), TopologyError> {
        let pane_id = target_pane
            .or_else(|| self.workspace.borrow().active_pane_id())
            .ok_or(TopologyError::PaneNotFound(PaneId(0)))?;
        let src_win = self.workspace.borrow().locate_pane(pane_id).map(|(w, _)| w);
        let (new_win, new_tab) = self.workspace.borrow_mut().detach_split(pane_id)?;

        if let Some(old_win) = src_win {
            self.reconcile_or_close_ui_window(old_win);
        }
        self.ensure_ui_window(new_win);
        self.sync_ui_window(new_win);
        Ok((new_win, new_tab, pane_id))
    }

    pub fn move_pane(
        self: &Rc<Self>,
        pane: PaneId,
        target_tab: TabId,
        axis: SplitAxis,
    ) -> Result<WindowId, TopologyError> {
        let src_win = self.workspace.borrow().locate_pane(pane).map(|(w, _)| w);
        let dst_win = self
            .workspace
            .borrow_mut()
            .move_pane_to_tab(pane, target_tab, axis)?;
        if let Some(old_win) = src_win
            && old_win != dst_win
        {
            self.reconcile_or_close_ui_window(old_win);
        }
        self.sync_ui_window(dst_win);
        Ok(dst_win)
    }

    /// Writes a divider ratio from a user drag. Deliberately does not rebuild
    /// the window: rebuilding mid-drag would fight the gesture and lose the
    /// handle position.
    pub fn set_split_ratio(&self, tab_id: TabId, path: &[bool], ratio: f64) -> bool {
        self.workspace
            .borrow_mut()
            .set_split_ratio(tab_id, path, ratio)
    }

    /// Drag-to-reorder: moves a tab within its window and rebuilds the strip.
    pub fn reorder_tab(self: &Rc<Self>, win_id: WindowId, tab_id: TabId, new_index: usize) {
        if self
            .workspace
            .borrow_mut()
            .reorder_tab(win_id, tab_id, new_index)
            .is_ok()
        {
            self.sync_ui_window(win_id);
        }
    }

    pub fn close_pane(self: &Rc<Self>, pane_id: PaneId) {
        self.save_before_last_window_goes();
        let src_win = self.workspace.borrow().locate_pane(pane_id).map(|(w, _)| w);
        let _dropped = self.workspace.borrow_mut().close_pane(pane_id);
        if let Some(win_id) = src_win {
            self.reconcile_or_close_ui_window(win_id);
        }
        self.quit_if_empty();
    }

    /// Closes a whole tab by closing its panes, so the same cascade applies:
    /// the last pane removes the tab, and the last tab removes the window.
    pub fn close_tab(self: &Rc<Self>, win_id: WindowId, tab_id: TabId) -> bool {
        let panes: Vec<PaneId> = {
            let ws = self.workspace.borrow();
            let Some(wm) = ws.window(win_id) else {
                return false;
            };
            let Some(tab) = wm.tabs.iter().find(|t| t.id == tab_id) else {
                return false;
            };
            tab.root.leaves()
        };
        if panes.is_empty() {
            return false;
        }
        for pid in panes {
            self.close_pane(pid);
        }
        true
    }

    /// Closes a whole window and every pane in it.
    pub fn close_window(self: &Rc<Self>, win_id: WindowId) -> bool {
        if self.workspace.borrow().window(win_id).is_none() {
            return false;
        }
        if let Some(ui) = self.ui_windows.borrow().get(&win_id).cloned() {
            ui.close_programmatically();
        }
        self.handle_window_closed(win_id);
        true
    }

    /// Saves the layout before a mutation that could empty the workspace.
    ///
    /// The exit save in `main` runs after the last window has been removed from
    /// the model, so on a normal quit it captured an EMPTY workspace and wiped
    /// the bundle. Closing the last window is how people actually quit, so that
    /// path silently lost the layout every time.
    fn save_before_last_window_goes(&self) {
        if self.workspace.borrow().windows().len() == 1 {
            let _ = self.save_session();
        }
    }

    pub fn handle_window_closed(&self, win_id: WindowId) {
        self.save_before_last_window_goes();
        self.ui_windows.borrow_mut().remove(&win_id);
        let _dropped = self.workspace.borrow_mut().close_window(win_id);
        self.quit_if_empty();
    }

    fn quit_if_empty(&self) {
        if self.workspace.borrow().windows().is_empty() {
            self.shutdown_ipc();
            if let Some(app) = self.gtk_app.borrow().as_ref() {
                app.quit();
            }
        }
    }

    pub fn execute_action(self: &Rc<Self>, action: ActionId, win_ctx: Option<WindowId>) {
        let active_win = win_ctx.or_else(|| self.workspace.borrow().active_window_id());
        let active_pane = active_win
            .and_then(|w| {
                self.workspace
                    .borrow()
                    .window(w)
                    .and_then(|wm| wm.active_pane())
            })
            .or_else(|| self.workspace.borrow().active_pane_id());

        match action {
            ActionId::NewWindow => {
                let _ = self.create_window(None);
            }
            ActionId::NewTab => {
                // New Tab follows the preferred multiplexer. `none` keeps the
                // plain-shell behaviour. The IPC SpawnTab verb deliberately
                // stays plain: agents and scripts ask for a shell explicitly.
                let preferred = self.config.borrow().preferred_mux;
                match preferred {
                    Some(kind) => {
                        let _ =
                            self.new_session(Some(kind), None, mux::SessionMode::Tab, active_win);
                    }
                    None => {
                        let _ = self.create_tab(active_win, None);
                    }
                }
            }
            ActionId::ClosePane => {
                if let Some(pid) = active_pane {
                    self.close_pane(pid);
                }
            }
            ActionId::SplitRight => {
                let _ = self.split_pane(active_pane, SplitAxis::Horizontal, None);
            }
            ActionId::SplitDown => {
                let _ = self.split_pane(active_pane, SplitAxis::Vertical, None);
            }
            ActionId::DetachSplit => {
                let _ = self.detach_split(active_pane);
            }
            ActionId::FocusNextPane => {
                if let Some(win_id) = active_win {
                    let _ = self
                        .workspace
                        .borrow_mut()
                        .cycle_pane_in_active_tab(win_id, true);
                    self.sync_ui_window(win_id);
                }
            }
            ActionId::FocusPrevPane => {
                if let Some(win_id) = active_win {
                    let _ = self
                        .workspace
                        .borrow_mut()
                        .cycle_pane_in_active_tab(win_id, false);
                    self.sync_ui_window(win_id);
                }
            }
            ActionId::UrgentNext => {
                let routed = self.workspace.borrow_mut().focus_urgent_next();
                if let Some((win_id, _, _)) = routed {
                    self.sync_ui_window(win_id);
                    if let Some(ui_win) = self.ui_windows.borrow().get(&win_id) {
                        ui_win.present();
                    }
                }
            }
            ActionId::Copy => {
                if let Some(pid) = active_pane {
                    let text = self
                        .workspace
                        .borrow()
                        .pane(pid)
                        .and_then(|e| e.selection_text());
                    if let (Some(text), Some(display)) = (text, gdk::Display::default()) {
                        display.clipboard().set_text(&text);
                    }
                }
            }
            ActionId::Paste => {
                if let (Some(pid), Some(display)) = (active_pane, gdk::Display::default()) {
                    let weak: Weak<Self> = Rc::downgrade(self);
                    display
                        .clipboard()
                        .read_text_async(gtk4::gio::Cancellable::NONE, move |res| {
                            if let (Ok(Some(text)), Some(ctrl)) = (res, weak.upgrade()) {
                                ctrl.paste_into_pane(pid, &text);
                            }
                        });
                }
            }
            ActionId::Search => {
                if let Some(win_id) = active_win
                    && let Some(ui_win) = self.ui_windows.borrow().get(&win_id).cloned()
                {
                    ui_win.show_search_bar();
                }
            }
            ActionId::ToggleMenuBar => {
                if let Some(win_id) = active_win {
                    let next = {
                        let mut ws = self.workspace.borrow_mut();
                        if let Some(w) = ws.window_mut(win_id) {
                            w.menubar_visible = !w.menubar_visible;
                            Some(w.menubar_visible)
                        } else {
                            None
                        }
                    };
                    if let Some(vis) = next {
                        self.default_menubar.set(vis);
                        self.sync_ui_window(win_id);
                    }
                }
            }
            ActionId::CommandPalette => {
                if let Some(win_id) = active_win
                    && let Some(ui_win) = self.ui_windows.borrow().get(&win_id).cloned()
                {
                    ui_win.show_command_palette();
                }
            }
            ActionId::Settings => {
                if let Some(win_id) = active_win
                    && let Some(ui_win) = self.ui_windows.borrow().get(&win_id).cloned()
                {
                    ui_win.show_settings_dialog();
                }
            }
            ActionId::SaveSession => {
                if let Err(err) = self.save_session() {
                    eprintln!("failed to save session: {err}");
                }
            }
            ActionId::CloseTab => {
                let target = active_win.and_then(|w| {
                    self.workspace
                        .borrow()
                        .window(w)
                        .map(|wm| (w, wm.active_tab))
                });
                if let Some((w, t)) = target {
                    self.close_tab(w, t);
                }
            }
            ActionId::CloseWindow => {
                if let Some(w) = active_win {
                    self.close_window(w);
                }
            }
            ActionId::NewSession => {
                if let Some(win_id) = active_win
                    && let Some(ui_win) = self.ui_windows.borrow().get(&win_id).cloned()
                {
                    ui_win.show_new_session_dialog();
                }
            }
            ActionId::SessionPicker => {
                if let Some(win_id) = active_win
                    && let Some(ui_win) = self.ui_windows.borrow().get(&win_id).cloned()
                {
                    ui_win.show_session_picker();
                }
            }
            ActionId::AgentAudit => {
                if let Some(win_id) = active_win
                    && let Some(ui_win) = self.ui_windows.borrow().get(&win_id).cloned()
                {
                    ui_win.show_agent_audit();
                }
            }
            ActionId::KeybindCheatsheet => {
                if let Some(win_id) = active_win
                    && let Some(ui_win) = self.ui_windows.borrow().get(&win_id).cloned()
                {
                    ui_win.show_cheatsheet();
                }
            }
            ActionId::JumpPrevPrompt => {
                if let Some(pid) = active_pane {
                    let moved = self
                        .workspace
                        .borrow_mut()
                        .pane_mut(pid)
                        .map(|e| e.jump_prev_prompt())
                        .unwrap_or(false);
                    if moved {
                        self.queue_redraw_pane(pid);
                    }
                }
            }
            ActionId::JumpNextPrompt => {
                if let Some(pid) = active_pane {
                    let moved = self
                        .workspace
                        .borrow_mut()
                        .pane_mut(pid)
                        .map(|e| e.jump_next_prompt())
                        .unwrap_or(false);
                    if moved {
                        self.queue_redraw_pane(pid);
                    }
                }
            }
        }
    }

    pub fn paste_into_pane(&self, pane_id: PaneId, text: &str) {
        let mut ws = self.workspace.borrow_mut();
        if let Some(eng) = ws.pane_mut(pane_id) {
            let formatted = eng.format_paste(text);
            let _ = eng.write_to_pty(&formatted);
        }
    }

    pub fn clear_pane_notice(&self, pane_id: PaneId) {
        if let Some(eng) = self.workspace.borrow_mut().pane_mut(pane_id) {
            eng.set_agent_notice(None);
        }
        self.refresh_all_window_chromes();
    }

    /// The live view for a window, if one exists. The controller owns these
    /// instances; a separately constructed `MutinyWindow` is not the one the
    /// tick refreshes.
    pub fn ui_window(&self, win_id: WindowId) -> Option<MutinyWindow> {
        self.ui_windows.borrow().get(&win_id).cloned()
    }

    fn ensure_ui_window(self: &Rc<Self>, win_id: WindowId) {
        if self.ui_windows.borrow().contains_key(&win_id) {
            return;
        }
        let Some(app) = self.gtk_app.borrow().clone() else {
            return;
        };
        let ui_win = MutinyWindow::new(&app, win_id, Rc::clone(self));
        // A window opened while an agent is already driving must show the
        // active icon at once, not wait for the next transition.
        ui_win.set_agent_active(self.agent_active());
        self.ui_windows.borrow_mut().insert(win_id, ui_win.clone());
        ui_win.present();
    }

    fn reconcile_or_close_ui_window(&self, win_id: WindowId) {
        let still_exists = self.workspace.borrow().window(win_id).is_some();
        if still_exists {
            self.sync_ui_window(win_id);
        } else if let Some(ui_win) = self.ui_windows.borrow_mut().remove(&win_id) {
            ui_win.close_programmatically();
        }
    }

    pub fn sync_ui_window(&self, win_id: WindowId) {
        let ui_win = self.ui_windows.borrow().get(&win_id).cloned();
        if let Some(w) = ui_win {
            w.rebuild_from_model();
        }
    }

    pub fn refresh_all_window_chromes(&self) {
        let wins: Vec<MutinyWindow> = self.ui_windows.borrow().values().cloned().collect();
        for w in wins {
            w.refresh_tab_strip_labels();
        }
    }

    pub fn queue_redraw_pane(&self, pane_id: PaneId) {
        let win_id = self.workspace.borrow().locate_pane(pane_id).map(|(w, _)| w);
        if let Some(wid) = win_id
            && let Some(ui_win) = self.ui_windows.borrow().get(&wid)
        {
            ui_win.queue_draw_pane(pane_id);
        }
    }

    /// Single non-blocking tick: drains PTYs, updates UI badges/draws, and services IPC/MCP sockets.
    pub fn poll_once(self: &Rc<Self>) {
        self.reload_config_if_changed();
        let pane_ids = self.workspace.borrow().pane_ids();
        let mut panes_exited = Vec::new();
        let mut windows_needing_chrome_refresh = Vec::new();

        for pid in pane_ids {
            let drain_outcome = {
                let mut ws = self.workspace.borrow_mut();
                ws.pane_mut(pid).and_then(|eng| eng.drain_pty().ok())
            };

            let Some((bytes_read, notifications)) = drain_outcome else {
                continue;
            };

            if bytes_read > 0 {
                self.queue_redraw_pane(pid);
            }

            for note in notifications {
                match note {
                    EngineNotification::DesktopNotification { title, body } => {
                        self.notifier.borrow().notify("mutiny", &title, &body);
                        if let Some((wid, _)) = self.workspace.borrow().locate_pane(pid)
                            && !windows_needing_chrome_refresh.contains(&wid)
                        {
                            windows_needing_chrome_refresh.push(wid);
                        }
                    }
                    EngineNotification::TitleChanged(_)
                    | EngineNotification::Bell
                    | EngineNotification::PromptReached { .. }
                    | EngineNotification::CommandFinished(_) => {
                        if let Some((wid, _)) = self.workspace.borrow().locate_pane(pid)
                            && !windows_needing_chrome_refresh.contains(&wid)
                        {
                            windows_needing_chrome_refresh.push(wid);
                        }
                    }
                    EngineNotification::ClipboardStore { primary, text } => {
                        if let Some(display) = gdk::Display::default() {
                            if primary {
                                display.primary_clipboard().set_text(&text);
                            } else {
                                display.clipboard().set_text(&text);
                            }
                        }
                    }
                    EngineNotification::ChildExited(_) => {
                        panes_exited.push(pid);
                    }
                }
            }
        }

        for wid in windows_needing_chrome_refresh {
            if let Some(ui_win) = self.ui_windows.borrow().get(&wid) {
                ui_win.refresh_tab_strip_labels();
            }
        }

        self.satisfy_pending_waits();
        self.advance_searches();
        self.advance_discoveries();
        self.advance_mux_commands();
        self.refresh_agent_icon();
        self.refresh_sessions_periodically();
        self.expire_pending_consents();

        for pid in panes_exited {
            self.close_pane(pid);
        }

        self.accept_ipc_clients();
    }

    /// Services both carriers. The operator socket assigns `Operator` to a
    /// same-uid peer; the agent socket assigns the mode the operator pinned at
    /// launch. Neither reads a mode from the request.
    fn accept_ipc_clients(self: &Rc<Self>) {
        self.accept_carrier(&self.ipc_listener, ConsentMode::Operator, "ctl", None);
        let agent_mode = self.agent_mode.get();
        self.accept_carrier(&self.agent_listener, agent_mode, "mcp-agent", None);
        self.accept_http_peer_carriers();
    }

    /// Services one carrier per HTTP peer token. Each assigns the identity its
    /// token was verified for, so two peers are distinct in the audit trail and
    /// in the grant set. The mode is the agent carrier's, never `Operator`.
    fn accept_http_peer_carriers(self: &Rc<Self>) {
        let peers = self.http_peers.borrow();
        for peer in peers.iter() {
            loop {
                let accepted = peer.listener.accept();
                match accepted {
                    Ok((stream, _)) => {
                        self.handle_accepted(stream, peer.mode, &peer.label, Some(peer.identity))
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
    }

    fn accept_carrier(
        self: &Rc<Self>,
        cell: &RefCell<Option<UnixListener>>,
        carrier: ConsentMode,
        default_label: &str,
        identity_override: Option<PeerIdentity>,
    ) {
        loop {
            let accepted = {
                let guard = cell.borrow();
                let Some(listener) = guard.as_ref() else {
                    break;
                };
                listener.accept()
            };

            match accepted {
                Ok((stream, _)) => {
                    self.handle_accepted(stream, carrier, default_label, identity_override)
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    fn handle_accepted(
        self: &Rc<Self>,
        mut stream: UnixStream,
        carrier: ConsentMode,
        default_label: &str,
        identity_override: Option<PeerIdentity>,
    ) {
        // Identity comes from the kernel, before anything the client wrote is
        // trusted. A peer from another uid is refused outright.
        let cred = match peer_credentials(&stream) {
            Ok(cred) if carrier_permits_peer(&cred, server_uid()) => cred,
            Ok(cred) => {
                let _ = write_response(
                    &mut stream,
                    &IpcResponse::err(format!(
                        "peer uid {} does not match server uid {}; connection refused",
                        cred.uid,
                        server_uid()
                    )),
                );
                return;
            }
            Err(err) => {
                let _ = write_response(
                    &mut stream,
                    &IpcResponse::err(format!("peer credential check failed: {err}")),
                );
                return;
            }
        };
        // A token-peer carrier assigns the identity it verified the token for.
        // The kernel check above still applies: the relaying process must be
        // same-uid. The remote client never chooses this identity, it only
        // presents a token, and the token-to-carrier mapping is fixed at launch.
        let peer = identity_override.unwrap_or_else(|| PeerIdentity::from_credentials(&cred));

        let env_req = match read_envelope(&mut stream) {
            Ok(env) => env,
            Err(err) => {
                let _ = write_response(&mut stream, &IpcResponse::err(err.to_string()));
                return;
            }
        };

        if let Some(resp) =
            self.evaluate_consent_and_dispatch(env_req, &stream, carrier, default_label, peer)
        {
            let _ = write_response(&mut stream, &resp);
        }
    }

    fn evaluate_consent_and_dispatch(
        self: &Rc<Self>,
        env_req: IpcEnvelope,
        raw_stream: &UnixStream,
        carrier: ConsentMode,
        default_label: &str,
        peer: PeerIdentity,
    ) -> Option<IpcResponse> {
        // The label is for display and audit only. It is not passed to the
        // consent decision, so it cannot influence whether a grant applies.
        let client_label = env_req
            .meta
            .client
            .clone()
            .unwrap_or_else(|| default_label.to_owned());

        if carrier != ConsentMode::Operator {
            *self.recent_agent.borrow_mut() = Some((client_label.clone(), Instant::now()));
            self.refresh_all_window_chromes();
        }

        let gui_present = !self.ui_windows.borrow().is_empty();
        let outcome = consent_outcome(
            env_req.request.consent_tier(),
            carrier,
            peer,
            &self.session_approved_clients.borrow(),
            gui_present,
        );

        match outcome {
            ConsentOutcome::Allow(decision) => {
                self.record_audit(&client_label, &env_req.request, decision, carrier, peer);
                self.execute_ipc_request(env_req.request, raw_stream)
            }
            ConsentOutcome::Deny(message) => {
                self.record_audit(
                    &client_label,
                    &env_req.request,
                    AuditDecision::Denied,
                    carrier,
                    peer,
                );
                Some(IpcResponse::err(message))
            }
            ConsentOutcome::Park => {
                let Ok(cloned) = raw_stream.try_clone() else {
                    return Some(IpcResponse::err("failed to clone IPC stream for consent"));
                };
                let cid = self.next_consent_id.get();
                self.next_consent_id.set(cid + 1);
                self.pending_consents
                    .borrow_mut()
                    .push_back(PendingConsent {
                        id: cid,
                        client: client_label,
                        request: env_req.request,
                        carrier,
                        peer,
                        deadline: Instant::now() + Duration::from_secs(25),
                        stream: cloned,
                    });
                self.refresh_all_window_chromes();
                None
            }
        }
    }

    fn execute_ipc_request(
        self: &Rc<Self>,
        req: IpcRequest,
        raw_stream: &UnixStream,
    ) -> Option<IpcResponse> {
        match req {
            IpcRequest::List => {
                let summary = self.workspace.borrow().summary();
                let mut resp = IpcResponse::empty_ok();
                if let IpcResponse::Ok { workspace, .. } = &mut resp {
                    *workspace = Some(summary);
                }
                Some(resp)
            }
            IpcRequest::Actions => {
                let mut resp = IpcResponse::empty_ok();
                if let IpcResponse::Ok { actions, .. } = &mut resp {
                    *actions = Some(action_catalog());
                }
                Some(resp)
            }
            IpcRequest::AuditLog { limit } => {
                let mut resp = IpcResponse::empty_ok();
                if let IpcResponse::Ok { audit, .. } = &mut resp {
                    *audit = Some(self.audit_entries(limit.clamp(1, MAX_AUDIT_ENTRIES)));
                }
                Some(resp)
            }
            IpcRequest::Snapshot { pane, max_lines } => {
                let pid = pane.or_else(|| self.workspace.borrow().active_pane_id());
                let Some(pid) = pid else {
                    return Some(IpcResponse::err("no active pane"));
                };
                let ws = self.workspace.borrow();
                let Some(eng) = ws.pane(pid) else {
                    return Some(IpcResponse::err(format!("pane {pid} not found")));
                };
                let snap = eng.snapshot(max_lines);
                let mut resp = ids_ok(None, None, Some(pid));
                if let IpcResponse::Ok {
                    snapshot,
                    prompt_seq,
                    output_seq,
                    ..
                } = &mut resp
                {
                    *prompt_seq = Some(snap.prompt_seq);
                    *output_seq = Some(snap.output_seq);
                    *snapshot = Some(snap);
                }
                Some(resp)
            }
            IpcRequest::ReadExited { pane } => {
                let ws = self.workspace.borrow();
                let mut resp = IpcResponse::empty_ok();
                if let IpcResponse::Ok {
                    exited,
                    exited_list,
                    ..
                } = &mut resp
                {
                    *exited = ws.read_exited(pane);
                    if pane.is_none() {
                        *exited_list = Some(ws.exited_panes());
                    }
                }
                Some(resp)
            }
            IpcRequest::Search {
                pane,
                pattern,
                ignore_case,
                max_matches,
            } => {
                let pid = pane.or_else(|| self.workspace.borrow().active_pane_id());
                let Some(pid) = pid else {
                    return Some(IpcResponse::err("no active pane"));
                };
                let ws = self.workspace.borrow();
                let Some(eng) = ws.pane(pid) else {
                    return Some(IpcResponse::err(format!("pane {pid} not found")));
                };
                // Chunked rather than scanned whole: a 10 000-line miss costs
                // about 3.2 ms, most of a 4 ms tick. The reply is parked and
                // completed across ticks; the wire shape is unchanged.
                let search = eng.new_search(&pattern, ignore_case, max_matches.clamp(1, 100));
                drop(ws);
                let Ok(cloned) = raw_stream.try_clone() else {
                    return Some(IpcResponse::err("failed to clone IPC stream for search"));
                };
                self.pending_searches.borrow_mut().push(PendingSearch {
                    pane: pid,
                    search,
                    deadline: Instant::now() + Duration::from_secs(30),
                    stream: cloned,
                });
                None
            }
            IpcRequest::NewSession {
                kind,
                name,
                mode,
                window,
            } => match self.new_session(kind, name, mode, window) {
                Ok((w, t, p)) => Some(ids_ok(Some(w), Some(t), Some(p))),
                Err(err) => Some(IpcResponse::err(err.to_string())),
            },
            IpcRequest::RenameSession {
                kind,
                name,
                new_name,
                host,
            } => match self.rename_session(kind, &name, &new_name, host.as_deref()) {
                Ok(()) => Some(IpcResponse::empty_ok()),
                Err(err) => Some(IpcResponse::err(err.to_string())),
            },
            IpcRequest::SaveSession => match self.save_session() {
                Ok(()) => Some(IpcResponse::empty_ok()),
                Err(err) => Some(IpcResponse::err(format!("could not save session: {err}"))),
            },
            IpcRequest::CloseTab { window, tab } => {
                let ws = self.workspace.borrow();
                let win = window.or_else(|| ws.active_window_id());
                let Some(win) = win else {
                    return Some(IpcResponse::err("no active window"));
                };
                let tab = tab.or_else(|| ws.window(win).map(|w| w.active_tab));
                drop(ws);
                let Some(tab) = tab else {
                    return Some(IpcResponse::err("no active tab"));
                };
                if self.close_tab(win, tab) {
                    Some(IpcResponse::empty_ok())
                } else {
                    Some(IpcResponse::err(format!("no such tab {}", tab.0)))
                }
            }
            IpcRequest::CloseWindow { window } => {
                let win = window.or_else(|| self.workspace.borrow().active_window_id());
                let Some(win) = win else {
                    return Some(IpcResponse::err("no active window"));
                };
                if self.close_window(win) {
                    Some(IpcResponse::empty_ok())
                } else {
                    Some(IpcResponse::err(format!("no such window {}", win.0)))
                }
            }
            IpcRequest::ClosePane { pane } => {
                let pid = pane.or_else(|| self.workspace.borrow().active_pane_id());
                let Some(pid) = pid else {
                    return Some(IpcResponse::err("no active pane"));
                };
                if self.workspace.borrow().pane(pid).is_none() {
                    return Some(IpcResponse::err(format!("no such pane {}", pid.0)));
                }
                self.close_pane(pid);
                Some(IpcResponse::empty_ok())
            }
            IpcRequest::Quit => {
                if let Err(err) = self.save_session() {
                    return Some(IpcResponse::err(format!("could not save session: {err}")));
                }
                // Defer the quit so this reply is written before the loop stops.
                if let Some(app) = self.gtk_app.borrow().clone() {
                    gtk4::glib::idle_add_local_once(move || app.quit());
                }
                Some(IpcResponse::empty_ok())
            }
            IpcRequest::ListSessions { host, refresh } => {
                // `host: None` is local only. Scanning configured remotes is the
                // GUI picker's job, where the human is the consent; an Observe
                // IPC call must not spend the user's SSH credentials.
                let targets = match host {
                    Some(host) => {
                        if let Err(err) = mux::validate_host(&host) {
                            return Some(IpcResponse::err(err));
                        }
                        vec![Some(host)]
                    }
                    None => vec![None],
                };
                let Ok(stream) = raw_stream.try_clone() else {
                    return Some(IpcResponse::err("could not clone the request stream"));
                };
                self.begin_discovery(targets, refresh, Some(stream))
            }
            IpcRequest::OpenSession {
                kind,
                name,
                host,
                mode,
                window,
                pane,
            } => match self.open_session(kind, &name, host.as_deref(), mode, window, pane) {
                Ok((w, t, p)) => Some(ids_ok(Some(w), Some(t), Some(p))),
                Err(err) => Some(IpcResponse::err(err.to_string())),
            },
            IpcRequest::PresentMedia {
                pane,
                path,
                cols,
                rows,
            } => {
                let pid = pane.or_else(|| self.workspace.borrow().active_pane_id());
                let Some(pid) = pid else {
                    return Some(IpcResponse::err("no active pane"));
                };
                let data = match std::fs::read(&path) {
                    Ok(data) => data,
                    Err(err) => {
                        return Some(IpcResponse::err(format!(
                            "cannot read {}: {err}",
                            path.display()
                        )));
                    }
                };
                let result = self
                    .workspace
                    .borrow_mut()
                    .pane_mut(pid)
                    .ok_or_else(|| format!("pane {pid} not found"))
                    .and_then(|eng| eng.present_image(data, cols.unwrap_or(0), rows.unwrap_or(0)));
                match result {
                    Ok((image_id, placement_id)) => {
                        self.queue_redraw_pane(pid);
                        let mut resp = ids_ok(None, None, Some(pid));
                        if let IpcResponse::Ok {
                            image_id: img,
                            placement_id: plc,
                            ..
                        } = &mut resp
                        {
                            *img = Some(image_id);
                            *plc = Some(placement_id);
                        }
                        Some(resp)
                    }
                    Err(err) => Some(IpcResponse::err(err)),
                }
            }
            IpcRequest::RunAction { action, window } => {
                self.execute_action(action, window);
                Some(IpcResponse::empty_ok())
            }
            IpcRequest::SpawnWindow { cwd } => match self.create_window(cwd) {
                Ok((w, t, p)) => Some(ids_ok(Some(w), Some(t), Some(p))),
                Err(e) => Some(IpcResponse::err(e.to_string())),
            },
            IpcRequest::SpawnTab { window, cwd } => match self.create_tab(window, cwd) {
                Ok((w, t, p)) => Some(ids_ok(Some(w), Some(t), Some(p))),
                Err(e) => Some(IpcResponse::err(e.to_string())),
            },
            IpcRequest::Split { pane, axis, cwd } => match self.split_pane(pane, axis, cwd) {
                Ok((w, t, p)) => Some(ids_ok(Some(w), Some(t), Some(p))),
                Err(e) => Some(IpcResponse::err(e.to_string())),
            },
            IpcRequest::Focus { pane } => {
                let res = self.workspace.borrow_mut().focus_pane(pane);
                match res {
                    Ok((w, t)) => {
                        self.sync_ui_window(w);
                        if let Some(ui_win) = self.ui_windows.borrow().get(&w) {
                            ui_win.present();
                        }
                        Some(ids_ok(Some(w), Some(t), Some(pane)))
                    }
                    Err(e) => Some(IpcResponse::err(e.to_string())),
                }
            }
            IpcRequest::DetachSplit { pane } => match self.detach_split(pane) {
                Ok((w, t, p)) => Some(ids_ok(Some(w), Some(t), Some(p))),
                Err(e) => Some(IpcResponse::err(e.to_string())),
            },
            IpcRequest::MovePane {
                pane,
                target_tab,
                axis,
            } => match self.move_pane(pane, target_tab, axis) {
                Ok(w) => Some(ids_ok(Some(w), Some(target_tab), Some(pane))),
                Err(e) => Some(IpcResponse::err(e.to_string())),
            },
            IpcRequest::SendText { pane, text } => {
                let pid = pane.or_else(|| self.workspace.borrow().active_pane_id());
                let Some(pid) = pid else {
                    return Some(IpcResponse::err("no active pane"));
                };
                let write_res = self
                    .workspace
                    .borrow_mut()
                    .pane_mut(pid)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pane not found"))
                    .and_then(|e| e.write_to_pty(text.as_bytes()));
                match write_res {
                    Ok(()) => Some(ids_ok(None, None, Some(pid))),
                    Err(e) => Some(IpcResponse::err(e.to_string())),
                }
            }
            IpcRequest::SendInput {
                pane,
                text,
                keys,
                bracketed_paste,
                return_snapshot,
            } => {
                let pid = pane.or_else(|| self.workspace.borrow().active_pane_id());
                let Some(pid) = pid else {
                    return Some(IpcResponse::err("no active pane"));
                };
                let mut payload = Vec::new();
                let snap_and_seqs = {
                    let mut ws = self.workspace.borrow_mut();
                    let Some(eng) = ws.pane_mut(pid) else {
                        return Some(IpcResponse::err(format!("pane {pid} not found")));
                    };
                    if let Some(body) = text.as_deref()
                        && !body.is_empty()
                    {
                        if bracketed_paste {
                            let clean = body.replace("\x1b[201~", "").replace("\x1b[200~", "");
                            payload.extend_from_slice(b"\x1b[200~");
                            payload.extend_from_slice(clean.as_bytes());
                            payload.extend_from_slice(b"\x1b[201~");
                        } else {
                            payload.extend_from_slice(body.as_bytes());
                        }
                    }
                    for key_name in &keys {
                        let Some(key_in) = parse_named_key(key_name) else {
                            return Some(IpcResponse::err(format!(
                                "unsupported key name: '{key_name}'"
                            )));
                        };
                        if let Some(seq) = eng.encode_key(key_in) {
                            payload.extend_from_slice(&seq);
                        }
                    }
                    if let Err(e) = eng.write_to_pty(&payload) {
                        return Some(IpcResponse::err(e.to_string()));
                    }
                    (
                        eng.prompt_seq(),
                        eng.output_seq(),
                        return_snapshot.then(|| eng.snapshot(Some(50))),
                    )
                };
                let mut resp = ids_ok(None, None, Some(pid));
                if let IpcResponse::Ok {
                    bytes_sent,
                    prompt_seq,
                    output_seq,
                    snapshot,
                    ..
                } = &mut resp
                {
                    *bytes_sent = Some(payload.len());
                    *prompt_seq = Some(snap_and_seqs.0);
                    *output_seq = Some(snap_and_seqs.1);
                    *snapshot = snap_and_seqs.2;
                }
                Some(resp)
            }
            IpcRequest::SendToAgent {
                pane,
                message,
                max_lines,
            } => {
                let pid = pane.or_else(|| self.workspace.borrow().active_pane_id());
                let Some(pid) = pid else {
                    return Some(IpcResponse::err("no active pane"));
                };
                let result = {
                    let mut ws = self.workspace.borrow_mut();
                    let Some(eng) = ws.pane_mut(pid) else {
                        return Some(IpcResponse::err(format!("pane {pid} not found")));
                    };
                    let mut payload = eng.format_paste(&message);
                    payload.push(b'\r');
                    let sent = payload.len();
                    if let Err(e) = eng.write_to_pty(&payload) {
                        return Some(IpcResponse::err(e.to_string()));
                    }
                    (
                        sent,
                        eng.prompt_seq(),
                        eng.output_seq(),
                        eng.snapshot(Some(max_lines.unwrap_or(50))),
                    )
                };
                let mut resp = ids_ok(None, None, Some(pid));
                if let IpcResponse::Ok {
                    bytes_sent,
                    prompt_seq,
                    output_seq,
                    snapshot,
                    ..
                } = &mut resp
                {
                    *bytes_sent = Some(result.0);
                    *prompt_seq = Some(result.1);
                    *output_seq = Some(result.2);
                    *snapshot = Some(result.3);
                }
                Some(resp)
            }
            IpcRequest::PresentNotice { pane, message } => {
                let pid = pane.or_else(|| self.workspace.borrow().active_pane_id());
                let Some(pid) = pid else {
                    return Some(IpcResponse::err("no active pane"));
                };
                {
                    let mut ws = self.workspace.borrow_mut();
                    let Some(eng) = ws.pane_mut(pid) else {
                        return Some(IpcResponse::err(format!("pane {pid} not found")));
                    };
                    eng.set_agent_notice(message);
                }
                self.refresh_all_window_chromes();
                Some(ids_ok(None, None, Some(pid)))
            }
            IpcRequest::LastOutput { pane } => {
                let pid = pane.or_else(|| self.workspace.borrow().active_pane_id());
                let Some(pid) = pid else {
                    return Some(IpcResponse::err("no active pane"));
                };
                let ws = self.workspace.borrow();
                let Some(eng) = ws.pane(pid) else {
                    return Some(IpcResponse::err(format!("pane {pid} not found")));
                };
                let mut resp = ids_ok(None, None, Some(pid));
                if let IpcResponse::Ok { command, .. } = &mut resp {
                    *command = eng.last_command().cloned();
                }
                Some(resp)
            }
            IpcRequest::ScreenText { pane } => {
                let pid = pane.or_else(|| self.workspace.borrow().active_pane_id());
                let Some(pid) = pid else {
                    return Some(IpcResponse::err("no active pane"));
                };
                let ws = self.workspace.borrow();
                let Some(eng) = ws.pane(pid) else {
                    return Some(IpcResponse::err(format!("pane {pid} not found")));
                };
                let mut resp = ids_ok(None, None, Some(pid));
                if let IpcResponse::Ok { text, .. } = &mut resp {
                    *text = Some(eng.screen_text());
                }
                Some(resp)
            }
            IpcRequest::WaitPrompt {
                pane,
                since_seq,
                timeout_ms,
            } => self.enqueue_wait(
                pane,
                since_seq,
                None,
                Duration::ZERO,
                timeout_ms,
                raw_stream,
            ),
            IpcRequest::AwaitTurn {
                pane,
                since_prompt_seq,
                since_output_seq,
                settle_ms,
                timeout_ms,
            } => {
                let settle = Duration::from_millis(settle_ms.clamp(20, 10_000));
                let cur_out = {
                    let pid = pane.or_else(|| self.workspace.borrow().active_pane_id());
                    pid.and_then(|p| self.workspace.borrow().pane(p).map(|e| e.output_seq()))
                };
                self.enqueue_wait(
                    pane,
                    since_prompt_seq,
                    Some(since_output_seq.unwrap_or_else(|| cur_out.unwrap_or(0))),
                    settle,
                    timeout_ms,
                    raw_stream,
                )
            }
        }
    }

    fn enqueue_wait(
        &self,
        pane: Option<PaneId>,
        since_prompt_seq: Option<u64>,
        since_output_seq: Option<u64>,
        settle: Duration,
        timeout_ms: u64,
        raw_stream: &UnixStream,
    ) -> Option<IpcResponse> {
        let pid = pane.or_else(|| self.workspace.borrow().active_pane_id());
        let Some(pid) = pid else {
            return Some(IpcResponse::err("no active pane"));
        };
        let baseline_prompt = {
            let ws = self.workspace.borrow();
            let Some(eng) = ws.pane(pid) else {
                return Some(IpcResponse::err(format!("pane {pid} not found")));
            };
            let cur_p = eng.prompt_seq();
            if let Some(since_p) = since_prompt_seq
                && cur_p > since_p
            {
                let mut resp = ids_ok(None, None, Some(pid));
                if let IpcResponse::Ok {
                    prompt_seq,
                    output_seq,
                    command,
                    snapshot,
                    ..
                } = &mut resp
                {
                    *prompt_seq = Some(cur_p);
                    *output_seq = Some(eng.output_seq());
                    *command = eng.last_command().cloned();
                    *snapshot = Some(eng.snapshot(Some(50)));
                }
                return Some(resp);
            }
            since_prompt_seq.unwrap_or(cur_p)
        };

        let Ok(cloned_stream) = raw_stream.try_clone() else {
            return Some(IpcResponse::err("failed to clone client stream"));
        };
        self.pending_waits.borrow_mut().push(PendingWait {
            pane: pid,
            since_prompt_seq: baseline_prompt,
            since_output_seq,
            settle,
            deadline: Instant::now() + Duration::from_millis(timeout_ms.max(50)),
            stream: cloned_stream,
        });
        None
    }

    /// Advances every active scan by a share of one tick's line budget. A
    /// full-history no-match scan therefore spreads across ticks instead of
    /// occupying one, no matter how many searches are in flight.
    fn advance_searches(self: &Rc<Self>) {
        let pane_ids = self.workspace.borrow().pane_ids();
        let ui_scans: Vec<PaneId> = pane_ids
            .into_iter()
            .filter(|pid| {
                self.workspace
                    .borrow()
                    .pane(*pid)
                    .and_then(|e| e.search())
                    .is_some_and(|s| !s.is_done())
            })
            .collect();
        let pending_count = self.pending_searches.borrow().len();
        let total = ui_scans.len() + pending_count;
        if total == 0 {
            return;
        }
        let per_scan = (SEARCH_LINES_PER_TICK / total).max(1);

        let mut ui_progressed = false;
        for pid in &ui_scans {
            let moved = self
                .workspace
                .borrow_mut()
                .pane_mut(*pid)
                .and_then(|e| e.advance_search(per_scan));
            if moved.is_some() {
                ui_progressed = true;
                self.queue_redraw_pane(*pid);
            }
        }

        let now = Instant::now();
        let mut remaining = Vec::new();
        let searches: Vec<PendingSearch> = self.pending_searches.borrow_mut().drain(..).collect();
        for mut item in searches {
            let outcome = {
                let ws = self.workspace.borrow();
                match ws.pane(item.pane) {
                    None => Err(String::from("pane closed while searching")),
                    Some(eng) => Ok(eng.advance_external_search(&mut item.search, per_scan)),
                }
            };
            match outcome {
                Err(msg) => {
                    let _ = write_response(&mut item.stream, &IpcResponse::err(msg));
                }
                Ok(progress) if progress.done => {
                    let mut resp = ids_ok(None, None, Some(item.pane));
                    if let IpcResponse::Ok { matches, .. } = &mut resp {
                        *matches = Some(item.search.matches().to_vec());
                    }
                    let _ = write_response(&mut item.stream, &resp);
                }
                Ok(_) if now >= item.deadline => {
                    let _ = write_response(&mut item.stream, &IpcResponse::err("search timed out"));
                }
                Ok(_) => remaining.push(item),
            }
        }
        *self.pending_searches.borrow_mut() = remaining;

        if ui_progressed {
            self.refresh_search_bars();
        }
    }

    /// Starts (or replaces) the viewport search on a pane. Empty queries are
    /// allowed so the bar stays open while the operator types.
    pub fn begin_pane_search(&self, pane: Option<PaneId>, query: &str) {
        let Some(pid) = pane.or_else(|| self.workspace.borrow().active_pane_id()) else {
            return;
        };
        if let Some(eng) = self.workspace.borrow_mut().pane_mut(pid) {
            eng.begin_search(query, true, 100);
        }
        self.refresh_search_bars();
    }

    pub fn cancel_pane_search(&self, pane: Option<PaneId>) {
        let Some(pid) = pane.or_else(|| self.workspace.borrow().active_pane_id()) else {
            return;
        };
        if let Some(eng) = self.workspace.borrow_mut().pane_mut(pid) {
            eng.cancel_search();
            eng.clear_selection();
        }
        self.queue_redraw_pane(pid);
        self.refresh_search_bars();
    }

    pub fn navigate_pane_search(&self, pane: Option<PaneId>, forward: bool) {
        let Some(pid) = pane.or_else(|| self.workspace.borrow().active_pane_id()) else {
            return;
        };
        let moved = self
            .workspace
            .borrow_mut()
            .pane_mut(pid)
            .and_then(|e| e.navigate_search(forward));
        if moved.is_some() {
            self.queue_redraw_pane(pid);
            self.refresh_search_bars();
        }
    }

    pub fn refresh_search_bars(&self) {
        let wins: Vec<MutinyWindow> = self.ui_windows.borrow().values().cloned().collect();
        for w in wins {
            w.refresh_search_bar();
        }
    }

    /// Refreshes open session pickers. Collects window clones first, exactly as
    /// `refresh_search_bars` does, because each window call re-borrows state.
    /// The window itself decides whether the generation moved, so this is cheap
    /// when nothing changed.
    pub fn refresh_session_pickers(&self) {
        let generation = self.cache_gen.get();
        if self.sessions_menu_gen.get() != generation {
            self.sessions_menu_gen.set(generation);
            let inventory = self.session_inventory();
            let menus: Vec<gtk4::gio::Menu> = self.sessions_menus.borrow().clone();
            for menu in menus {
                crate::action::populate_sessions_menu(&menu, &inventory);
            }
        }
        let wins: Vec<MutinyWindow> = self.ui_windows.borrow().values().cloned().collect();
        for w in wins {
            w.refresh_session_picker(generation);
        }
    }

    fn satisfy_pending_waits(&self) {
        let now = Instant::now();
        let mut remaining = Vec::new();
        let waits: Vec<PendingWait> = self.pending_waits.borrow_mut().drain(..).collect();

        for mut w in waits {
            let ready = {
                let ws = self.workspace.borrow();
                match ws.pane(w.pane) {
                    None => Err("pane closed while waiting"),
                    Some(eng) => {
                        let prompt_ready = eng.prompt_seq() > w.since_prompt_seq;
                        let quiescence_ready = w.since_output_seq.is_some_and(|s_out| {
                            eng.has_turn_settled(w.since_prompt_seq, s_out, w.settle)
                        });
                        if prompt_ready || quiescence_ready {
                            Ok(Some((
                                eng.prompt_seq(),
                                eng.output_seq(),
                                eng.last_command().cloned(),
                                eng.snapshot(Some(50)),
                            )))
                        } else {
                            Ok(None)
                        }
                    }
                }
            };

            match ready {
                Err(msg) => {
                    let _ = write_response(&mut w.stream, &IpcResponse::err(msg));
                }
                Ok(Some((p_seq, o_seq, last_cmd, snap))) => {
                    let mut resp = ids_ok(None, None, Some(w.pane));
                    if let IpcResponse::Ok {
                        prompt_seq,
                        output_seq,
                        command,
                        snapshot,
                        ..
                    } = &mut resp
                    {
                        *prompt_seq = Some(p_seq);
                        *output_seq = Some(o_seq);
                        *command = last_cmd;
                        *snapshot = Some(snap);
                    }
                    let _ = write_response(&mut w.stream, &resp);
                }
                Ok(None) if now >= w.deadline => {
                    let _ = write_response(&mut w.stream, &IpcResponse::err("wait timed out"));
                }
                Ok(None) => remaining.push(w),
            }
        }

        *self.pending_waits.borrow_mut() = remaining;
    }

    fn expire_pending_consents(&self) {
        let now = Instant::now();
        let mut expired_any = false;
        let mut queue = self.pending_consents.borrow_mut();
        let mut kept = VecDeque::new();
        while let Some(mut item) = queue.pop_front() {
            if now >= item.deadline {
                expired_any = true;
                let _ = write_response(
                    &mut item.stream,
                    &IpcResponse::err("consent prompt timed out"),
                );
            } else {
                kept.push_back(item);
            }
        }
        *queue = kept;
        drop(queue);
        if expired_any {
            self.refresh_all_window_chromes();
        }
    }
}

/// Appends one redacted entry to the durable trail. Best-effort: a failing
/// audit write must not take down the terminal, but it is reported on stderr
/// so the operator can see the trail is incomplete.
fn append_audit_entry(path: &Path, entry: &AuditEntry) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > AUDIT_ROTATE_BYTES
    {
        let _ = std::fs::rename(path, path.with_extension("jsonl.1"));
    }
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    match opened {
        Ok(mut file) => {
            if let Ok(line) = serde_json::to_string(entry)
                && let Err(err) = writeln!(file, "{line}")
            {
                eprintln!("warning: audit append failed: {err}");
            }
        }
        Err(err) => eprintln!("warning: cannot open audit log {}: {err}", path.display()),
    }
}

/// Loads the tail of the durable trail into the viewer ring and returns the
/// next sequence number. Corrupt lines are skipped rather than fatal.
fn load_audit_tail(path: &Path, ring: &mut VecDeque<AuditEntry>) -> u64 {
    let Ok(file) = std::fs::File::open(path) else {
        return 1;
    };
    let mut max_seq = 0u64;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        let Ok(entry) = serde_json::from_str::<AuditEntry>(&line) else {
            continue;
        };
        max_seq = max_seq.max(entry.seq);
        if ring.len() >= MAX_AUDIT_ENTRIES {
            ring.pop_front();
        }
        ring.push_back(entry);
    }
    max_seq + 1
}

fn ids_ok(window: Option<WindowId>, tab: Option<TabId>, pane: Option<PaneId>) -> IpcResponse {
    IpcResponse::Ok {
        workspace: None,
        actions: None,
        window,
        tab,
        pane,
        command: None,
        snapshot: None,
        exited: None,
        exited_list: None,
        matches: None,
        audit: None,
        bytes_sent: None,
        text: None,
        prompt_seq: None,
        output_seq: None,
        image_id: None,
        placement_id: None,
        sessions: None,
    }
}

/// Wall-clock milliseconds for the inventory timestamp.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ssh_config_path() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".ssh").join("config"))
        .unwrap_or_else(|_| PathBuf::from(".ssh/config"))
}

/// Pipes must be nonblocking so a tick never waits on a child.
fn set_nonblocking(fd: i32) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Drains up to the shared per-tick budget. Bytes past `MAX_DISCOVERY_BYTES`
/// are read and discarded rather than left in the pipe: a child that fills the
/// pipe buffer would block forever and `try_wait` would never fire.
fn drain_bounded<R: Read>(reader: &mut R, sink: &mut Vec<u8>, budget: &mut usize) {
    let mut buf = [0u8; 8192];
    while *budget > 0 {
        let want = (*budget).min(buf.len());
        match reader.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => {
                *budget -= n;
                if sink.len() < MAX_DISCOVERY_BYTES {
                    let keep = (MAX_DISCOVERY_BYTES - sink.len()).min(n);
                    sink.extend_from_slice(&buf[..keep]);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// Turns one child's output into sessions or an actionable error. A non-zero
/// exit with no sessions is normal for tmux when no server is running, so it is
/// reported as "no sessions" rather than a hard failure.
fn parse_discovery(
    kind: MuxKind,
    stdout: &str,
    stderr: &str,
    code: Option<i32>,
    host: Option<&str>,
) -> Result<Vec<MuxSession>, String> {
    let sessions = match kind {
        MuxKind::Tmux => mux::parse_tmux(stdout, host),
        MuxKind::Zellij => mux::parse_zellij(stdout, host),
    };
    if !sessions.is_empty() {
        return Ok(sessions);
    }
    if code == Some(0) {
        return Ok(sessions);
    }
    Err(mux::classify_failure(kind.label(), code, stderr))
}
