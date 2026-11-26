use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::action::by_slug;
use crate::ipc::{
    CallerMeta, ConsentMode, IPC_PROTOCOL_VERSION, IpcEnvelope, IpcRequest, IpcResponse,
    send_envelope,
};
use crate::topology::{PaneId, SplitAxis, TabId, WindowId};

const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";
/// MCP revisions this bridge implements. `initialize` negotiates against this
/// set instead of echoing whatever the client asked for.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// Negotiates an MCP protocol revision against the set Mutiny implements.
pub fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    let ver = requested.unwrap_or(DEFAULT_PROTOCOL_VERSION);
    if SUPPORTED_PROTOCOL_VERSIONS.contains(&ver) {
        SUPPORTED_PROTOCOL_VERSIONS
            .iter()
            .copied()
            .find(|&v| v == ver)
            .unwrap_or(DEFAULT_PROTOCOL_VERSION)
    } else {
        DEFAULT_PROTOCOL_VERSION
    }
}

/// Resource URIs that exist regardless of workspace contents. Single source
/// for `resources/list`, `resources/read`, and the parity test.
pub const STATIC_RESOURCES: &[&str] = &[
    "ui://mutiny/workspace",
    "ui://mutiny/actions",
    "ui://mutiny/exited",
    "ui://mutiny/audit",
];

/// How an IPC verb is reachable from MCP. The match in `mcp_surface_for` is
/// exhaustive, so a new verb cannot compile until its reachability is decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpSurface {
    Tool(&'static str),
    Resource(&'static str),
}

/// Maps every IPC verb to the MCP tool or resource that reaches it. `None`
/// means the verb is CLI-only, which the parity test treats as a failure.
pub fn mcp_surface_for(req: &IpcRequest) -> Option<McpSurface> {
    match req {
        IpcRequest::List => Some(McpSurface::Tool("list_workspace")),
        IpcRequest::Actions => Some(McpSurface::Tool("list_actions")),
        IpcRequest::AuditLog { .. } => Some(McpSurface::Resource("ui://mutiny/audit")),
        IpcRequest::Snapshot { .. } => Some(McpSurface::Tool("read_pane_snapshot")),
        IpcRequest::ReadExited { .. } => Some(McpSurface::Tool("read_exited_pane")),
        IpcRequest::Search { .. } => Some(McpSurface::Tool("search_scrollback")),
        IpcRequest::RunAction { .. } => Some(McpSurface::Tool("run_action")),
        IpcRequest::SpawnWindow { .. } => Some(McpSurface::Tool("spawn_pane")),
        IpcRequest::SpawnTab { .. } => Some(McpSurface::Tool("spawn_pane")),
        IpcRequest::Split { .. } => Some(McpSurface::Tool("spawn_pane")),
        IpcRequest::Focus { .. } => Some(McpSurface::Tool("focus_pane")),
        IpcRequest::DetachSplit { .. } => Some(McpSurface::Tool("detach_split")),
        IpcRequest::MovePane { .. } => Some(McpSurface::Tool("move_pane")),
        IpcRequest::SendText { .. } => Some(McpSurface::Tool("send_terminal_input")),
        IpcRequest::SendInput { .. } => Some(McpSurface::Tool("send_terminal_input")),
        IpcRequest::SendToAgent { .. } => Some(McpSurface::Tool("send_to_agent")),
        IpcRequest::PresentNotice { .. } => Some(McpSurface::Tool("present_notice")),
        IpcRequest::LastOutput { .. } => Some(McpSurface::Tool("read_last_command_output")),
        IpcRequest::ScreenText { .. } => Some(McpSurface::Tool("read_screen_text")),
        IpcRequest::WaitPrompt { .. } => Some(McpSurface::Tool("wait_prompt")),
        IpcRequest::AwaitTurn { .. } => Some(McpSurface::Tool("await_turn")),
        IpcRequest::PresentMedia { .. } => Some(McpSurface::Tool("present_media")),
        IpcRequest::ListSessions { .. } => Some(McpSurface::Tool("list_sessions")),
        IpcRequest::SessionTree { .. } => Some(McpSurface::Tool("session_tree")),
        IpcRequest::OpenSession { .. } => Some(McpSurface::Tool("open_session")),
        IpcRequest::OpenSessionGrid { .. } => Some(McpSurface::Tool("open_session_grid")),
        IpcRequest::HandoffToPeer { .. } => Some(McpSurface::Tool("handoff_to_peer")),
        IpcRequest::PresentToPeer { .. } => Some(McpSurface::Tool("present_to_peer")),
        IpcRequest::SaveSession => Some(McpSurface::Tool("save_session")),
        IpcRequest::ToggleQuick => Some(McpSurface::Tool("toggle_quick")),
        IpcRequest::NewSession { .. } => Some(McpSurface::Tool("new_session")),
        IpcRequest::RenameSession { .. } => Some(McpSurface::Tool("rename_session")),
        IpcRequest::ProposePolicy { .. } => Some(McpSurface::Tool("propose_policy")),
        IpcRequest::ListPolicies => Some(McpSurface::Tool("list_policies")),
        IpcRequest::InstallPolicy { .. } => Some(McpSurface::Tool("install_policy")),
        IpcRequest::RevokePolicy { .. } => Some(McpSurface::Tool("revoke_policy")),
        IpcRequest::ListPeers => Some(McpSurface::Tool("list_peers")),
        IpcRequest::ListPeerAlerts => Some(McpSurface::Tool("list_peer_alerts")),
        IpcRequest::ClearPeerAlerts => Some(McpSurface::Tool("clear_peer_alerts")),
        IpcRequest::PairHaven { .. } => Some(McpSurface::Tool("pair_haven")),
        IpcRequest::EnrollHavenSigner { .. } => Some(McpSurface::Tool("enroll_haven_signer")),
        IpcRequest::RevokePeer { .. } => Some(McpSurface::Tool("revoke_peer")),
        IpcRequest::EndSession { .. } => Some(McpSurface::Tool("end_session")),
        IpcRequest::ClosePane { .. } => Some(McpSurface::Tool("close_pane")),
        IpcRequest::CloseTab { .. } => Some(McpSurface::Tool("close_tab")),
        IpcRequest::CloseWindow { .. } => Some(McpSurface::Tool("close_window")),
        IpcRequest::Quit => Some(McpSurface::Tool("quit")),
    }
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

pub struct McpBridge {
    socket_path: PathBuf,
    consent_mode: ConsentMode,
    client_name: String,
}

impl McpBridge {
    pub fn new(
        socket_path: PathBuf,
        consent_mode: ConsentMode,
        default_client_name: String,
    ) -> Self {
        Self {
            socket_path,
            consent_mode,
            client_name: default_client_name,
        }
    }

    pub fn serve_stdio(&mut self) -> io::Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut reader = stdin.lock();
        let mut writer = stdout.lock();
        self.serve(&mut reader, &mut writer)
    }

    pub fn serve<R: BufRead, W: Write>(
        &mut self,
        reader: &mut R,
        writer: &mut W,
    ) -> io::Result<()> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break;
            }
            if let Some(reply) = self.handle_line(&line) {
                writer.write_all(reply.as_bytes())?;
                writer.write_all(b"\n")?;
                writer.flush()?;
            }
        }
        Ok(())
    }

    /// Handles one JSON-RPC line and returns the serialized reply, or None for
    /// a notification or blank line. Shared by the stdio transport and the
    /// HTTP carrier so both speak identical JSON-RPC.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(req) => req,
            Err(err) => {
                let reply = JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("invalid JSON-RPC payload: {err}"),
                    }),
                };
                return serde_json::to_string(&reply).ok();
            }
        };
        // Notifications (e.g. notifications/initialized) produce no response.
        let id = req.id.clone()?;
        let reply = match self.dispatch(&req.method, &req.params) {
            Ok(result) => JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(result),
                error: None,
            },
            Err((code, message)) => JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: None,
                error: Some(JsonRpcError { code, message }),
            },
        };
        serde_json::to_string(&reply).ok()
    }

    fn call_ipc(
        &self,
        request: IpcRequest,
        timeout: Duration,
    ) -> Result<IpcResponse, (i32, String)> {
        // The launch flag is a local ceiling, enforced here before anything
        // reaches the socket. The carrier assigns the effective mode; this
        // bridge can only ever be more restrictive than the carrier, never less.
        if self.consent_mode == ConsentMode::ObserveOnly
            && request.consent_tier() != crate::action::ConsentTier::Observe
        {
            let label = match request.consent_tier() {
                crate::action::ConsentTier::Irreversible => "Tier-4 Irreversible",
                crate::action::ConsentTier::PolicyChange => "PolicyChange",
                _ => "Tier-2 Act",
            };
            return Ok(IpcResponse::err(format!(
                "observe-only session cannot execute {label} verbs"
            )));
        }

        let env_req = IpcEnvelope {
            version: IPC_PROTOCOL_VERSION,
            meta: CallerMeta {
                client: Some(self.client_name.clone()),
            },
            request,
        };
        send_envelope(&self.socket_path, &env_req, timeout).map_err(|e| {
            (
                -32000,
                format!(
                    "failed to reach mutiny at {}: {e}",
                    self.socket_path.display()
                ),
            )
        })
    }

    fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value, (i32, String)> {
        match method {
            "initialize" => {
                if let Some(name) = params
                    .get("clientInfo")
                    .and_then(|ci| ci.get("name"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    self.client_name = name.trim().to_owned();
                }
                // Negotiate: honour a revision we implement, otherwise answer
                // with our own latest. Never echo an unsupported version back.
                let negotiated = negotiate_protocol_version(
                    params.get("protocolVersion").and_then(Value::as_str),
                );
                Ok(json!({
                    "protocolVersion": negotiated,
                    "capabilities": {
                        "tools": {},
                        "resources": {}
                    },
                    "serverInfo": {
                        "name": "mutiny",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }))
            }
            "ping" => Ok(json!({})),
            "resources/list" => self.list_resources(),
            "resources/read" => self.read_resource(params),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => self.call_tool(params),
            other => Err((-32601, format!("method not found: {other}"))),
        }
    }

    fn list_resources(&self) -> Result<Value, (i32, String)> {
        let mut resources: Vec<Value> = STATIC_RESOURCES
            .iter()
            .map(|uri| {
                let (name, description) = resource_metadata(uri);
                json!({
                    "uri": uri,
                    "name": name,
                    "description": description,
                    "mimeType": "application/json"
                })
            })
            .collect();

        if let Ok(IpcResponse::Ok {
            workspace: Some(ws),
            ..
        }) = self.call_ipc(IpcRequest::List, Duration::from_secs(3))
        {
            for win in ws.windows {
                for tab in win.tabs {
                    for pane in tab.panes {
                        resources.push(json!({
                            "uri": format!("ui://mutiny/pane/{}/snapshot", pane.id.0),
                            "name": format!("Pane {} ({})", pane.id.0, pane.title),
                            "description": "Structured terminal screen, cursor, modes, and OSC metadata",
                            "mimeType": "application/json"
                        }));
                    }
                }
            }
        }

        Ok(json!({ "resources": resources }))
    }

    fn read_resource(&self, params: &Value) -> Result<Value, (i32, String)> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| (-32602, String::from("missing `uri` string parameter")))?;

        let ipc_req = if uri == "ui://mutiny/workspace" {
            IpcRequest::List
        } else if uri == "ui://mutiny/actions" {
            IpcRequest::Actions
        } else if uri == "ui://mutiny/exited" {
            IpcRequest::ReadExited { pane: None }
        } else if uri == "ui://mutiny/audit" {
            IpcRequest::AuditLog { limit: 100 }
        } else if let Some(rest) = uri.strip_prefix("ui://mutiny/pane/") {
            let pid_str = rest
                .strip_suffix("/snapshot")
                .ok_or_else(|| (-32602, format!("unsupported pane resource URI: {uri}")))?;
            let pid = pid_str
                .parse::<u64>()
                .map_err(|_| (-32602, format!("invalid pane id in URI: {uri}")))?;
            IpcRequest::Snapshot {
                pane: Some(PaneId(pid)),
                max_lines: None,
            }
        } else {
            return Err((-32602, format!("unknown resource URI: {uri}")));
        };

        let resp = self.call_ipc(ipc_req, Duration::from_secs(5))?;
        let text = serde_json::to_string_pretty(&resp)
            .map_err(|e| (-32603, format!("serialize error: {e}")))?;

        Ok(json!({
            "contents": [{
                "uri": uri,
                "mimeType": "application/json",
                "text": text
            }]
        }))
    }

    fn call_tool(&self, params: &Value) -> Result<Value, (i32, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| (-32602, String::from("missing tool `name`")))?;
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let opt_pane = |key: &str| args.get(key).and_then(Value::as_u64).map(PaneId);
        let opt_win = |key: &str| args.get(key).and_then(Value::as_u64).map(WindowId);
        let opt_cwd = || {
            args.get("cwd")
                .and_then(Value::as_str)
                .map(Path::new)
                .map(Path::to_path_buf)
        };

        let (ipc_req, timeout) = match name {
            "list_workspace" => (IpcRequest::List, Duration::from_secs(5)),
            "list_actions" => (IpcRequest::Actions, Duration::from_secs(5)),
            "read_screen_text" => (
                IpcRequest::ScreenText {
                    pane: opt_pane("pane"),
                },
                Duration::from_secs(5),
            ),
            "wait_prompt" => {
                let timeout_ms = args
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(15_000);
                (
                    IpcRequest::WaitPrompt {
                        pane: opt_pane("pane"),
                        since_seq: args.get("since_seq").and_then(Value::as_u64),
                        timeout_ms,
                    },
                    Duration::from_millis(timeout_ms.saturating_add(2_000)),
                )
            }
            "read_pane_snapshot" => (
                IpcRequest::Snapshot {
                    pane: opt_pane("pane"),
                    max_lines: args
                        .get("max_lines")
                        .and_then(Value::as_u64)
                        .map(|n| n as usize),
                },
                Duration::from_secs(5),
            ),
            "read_last_command_output" => (
                IpcRequest::LastOutput {
                    pane: opt_pane("pane"),
                },
                Duration::from_secs(5),
            ),
            "read_exited_pane" => (
                IpcRequest::ReadExited {
                    pane: opt_pane("pane"),
                },
                Duration::from_secs(5),
            ),
            "search_scrollback" => {
                let pattern = args
                    .get("pattern")
                    .and_then(Value::as_str)
                    .ok_or_else(|| (-32602, String::from("`pattern` is required")))?
                    .to_owned();
                let ignore_case = args
                    .get("ignore_case")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let max_matches = args
                    .get("max_matches")
                    .and_then(Value::as_u64)
                    .unwrap_or(20) as usize;
                (
                    IpcRequest::Search {
                        pane: opt_pane("pane"),
                        pattern,
                        ignore_case,
                        max_matches,
                    },
                    Duration::from_secs(5),
                )
            }
            "await_turn" => {
                let timeout_ms = args
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(30_000);
                let settle_ms = args.get("settle_ms").and_then(Value::as_u64).unwrap_or(250);
                (
                    IpcRequest::AwaitTurn {
                        pane: opt_pane("pane"),
                        since_prompt_seq: args.get("since_prompt_seq").and_then(Value::as_u64),
                        since_output_seq: args.get("since_output_seq").and_then(Value::as_u64),
                        settle_ms,
                        timeout_ms,
                    },
                    Duration::from_millis(timeout_ms.saturating_add(2_000)),
                )
            }
            "send_terminal_input" => {
                let text = args.get("text").and_then(Value::as_str).map(str::to_owned);
                let keys = args
                    .get("keys")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let bracketed_paste = args
                    .get("bracketed_paste")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let return_snapshot = args
                    .get("return_snapshot")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                (
                    IpcRequest::SendInput {
                        pane: opt_pane("pane"),
                        text,
                        keys,
                        bracketed_paste,
                        return_snapshot,
                    },
                    Duration::from_secs(70),
                )
            }
            "send_to_agent" => {
                let message = args
                    .get("message")
                    .and_then(Value::as_str)
                    .ok_or_else(|| (-32602, String::from("`message` is required")))?
                    .to_owned();
                let max_lines = args
                    .get("max_lines")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize);
                (
                    IpcRequest::SendToAgent {
                        pane: opt_pane("pane"),
                        message,
                        max_lines,
                    },
                    Duration::from_secs(70),
                )
            }
            "spawn_pane" => {
                let mode = args
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("split-right");
                let req = match mode {
                    "window" => IpcRequest::SpawnWindow { cwd: opt_cwd() },
                    "tab" => IpcRequest::SpawnTab {
                        window: opt_win("window"),
                        cwd: opt_cwd(),
                    },
                    "split-down" => IpcRequest::Split {
                        pane: opt_pane("pane"),
                        axis: SplitAxis::Vertical,
                        cwd: opt_cwd(),
                    },
                    _ => IpcRequest::Split {
                        pane: opt_pane("pane"),
                        axis: SplitAxis::Horizontal,
                        cwd: opt_cwd(),
                    },
                };
                (req, Duration::from_secs(70))
            }
            "detach_split" => (
                IpcRequest::DetachSplit {
                    pane: opt_pane("pane"),
                },
                Duration::from_secs(70),
            ),
            "move_pane" => {
                let pane =
                    opt_pane("pane").ok_or_else(|| (-32602, String::from("`pane` is required")))?;
                let target_tab = args
                    .get("target_tab")
                    .and_then(Value::as_u64)
                    .map(TabId)
                    .ok_or_else(|| (-32602, String::from("`target_tab` is required")))?;
                let down = args.get("down").and_then(Value::as_bool).unwrap_or(false);
                (
                    IpcRequest::MovePane {
                        pane,
                        target_tab,
                        axis: if down {
                            SplitAxis::Vertical
                        } else {
                            SplitAxis::Horizontal
                        },
                    },
                    Duration::from_secs(70),
                )
            }
            "focus_pane" => {
                let pane =
                    opt_pane("pane").ok_or_else(|| (-32602, String::from("`pane` is required")))?;
                (IpcRequest::Focus { pane }, Duration::from_secs(70))
            }
            "run_action" => {
                let slug = args
                    .get("slug")
                    .and_then(Value::as_str)
                    .ok_or_else(|| (-32602, String::from("`slug` is required")))?;
                let spec = by_slug(slug)
                    .ok_or_else(|| (-32602, format!("unknown action slug: {slug}")))?;
                (
                    IpcRequest::RunAction {
                        action: spec.id,
                        window: opt_win("window"),
                    },
                    Duration::from_secs(70),
                )
            }
            "save_session" => (IpcRequest::SaveSession, Duration::from_secs(70)),
            "toggle_quick" => (IpcRequest::ToggleQuick, Duration::from_secs(70)),
            "new_session" => {
                let kind = match args.get("kind").and_then(Value::as_str) {
                    None | Some("tmux") => Some(crate::mux::MuxKind::Tmux),
                    Some("zellij") => Some(crate::mux::MuxKind::Zellij),
                    Some("none") => None,
                    Some(other) => {
                        return Err((
                            -32602,
                            format!("unknown kind {other:?}: tmux, zellij, or none"),
                        ));
                    }
                };
                let mode_text = args.get("mode").and_then(Value::as_str).unwrap_or("tab");
                let mode = crate::mux::SessionMode::parse(mode_text)
                    .ok_or_else(|| (-32602, format!("unknown mode {mode_text:?}")))?;
                (
                    IpcRequest::NewSession {
                        kind,
                        name: args.get("name").and_then(Value::as_str).map(str::to_owned),
                        mode,
                        window: opt_win("window"),
                    },
                    Duration::from_secs(70),
                )
            }
            "list_policies" => (IpcRequest::ListPolicies, Duration::from_secs(10)),
            "install_policy" => (
                IpcRequest::InstallPolicy {
                    id: args.get("id").and_then(Value::as_u64).unwrap_or(0),
                },
                Duration::from_secs(70),
            ),
            "revoke_policy" => (
                IpcRequest::RevokePolicy {
                    id: args.get("id").and_then(Value::as_u64).unwrap_or(0),
                },
                Duration::from_secs(70),
            ),
            "list_peers" => (IpcRequest::ListPeers, Duration::from_secs(5)),
            "list_peer_alerts" => (IpcRequest::ListPeerAlerts, Duration::from_secs(5)),
            "clear_peer_alerts" => (IpcRequest::ClearPeerAlerts, Duration::from_secs(15)),
            "pair_haven" => {
                let label = args
                    .get("label")
                    .and_then(Value::as_str)
                    .ok_or_else(|| (-32602, String::from("`label` is required")))?
                    .to_owned();
                let url = args
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| (-32602, String::from("`url` is required")))?
                    .to_owned();
                let token = args
                    .get("token")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_owned);
                let enroll_signer = args
                    .get("enroll_signer")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let timeout = match (token.is_some(), enroll_signer) {
                    (true, false) => Duration::from_secs(20),
                    (false, true) => Duration::from_secs(135),
                    _ => Duration::from_secs(70),
                };
                (
                    IpcRequest::PairHaven {
                        label,
                        url,
                        token,
                        enroll_signer,
                    },
                    timeout,
                )
            }
            "enroll_haven_signer" => {
                let id_or_label = args
                    .get("id_or_label")
                    .and_then(Value::as_str)
                    .ok_or_else(|| (-32602, String::from("`id_or_label` is required")))?
                    .to_owned();
                (
                    IpcRequest::EnrollHavenSigner { id_or_label },
                    Duration::from_secs(70),
                )
            }
            "revoke_peer" => {
                let id_or_label = args
                    .get("id_or_label")
                    .and_then(Value::as_str)
                    .ok_or_else(|| (-32602, String::from("`id_or_label` is required")))?
                    .to_owned();
                (
                    IpcRequest::RevokePeer { id_or_label },
                    Duration::from_secs(70),
                )
            }
            "propose_policy" => {
                let description = args
                    .get("description")
                    .and_then(Value::as_str)
                    .ok_or_else(|| (-32602, String::from("`description` is required")))?
                    .to_owned();
                let verbs = args
                    .get("verbs")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let arg_constraints = args
                    .get("arg_constraints")
                    .and_then(Value::as_object)
                    .map(|o| {
                        o.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                            .collect()
                    })
                    .unwrap_or_default();
                (
                    IpcRequest::ProposePolicy {
                        description,
                        verbs,
                        arg_constraints,
                        max_calls_per_minute: args
                            .get("max_calls_per_minute")
                            .and_then(Value::as_u64)
                            .unwrap_or(60) as u32,
                        expires_in_minutes: args
                            .get("expires_in_minutes")
                            .and_then(Value::as_u64)
                            .unwrap_or(60),
                    },
                    Duration::from_secs(10),
                )
            }
            "end_session" => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| (-32602, String::from("`name` is required")))?
                    .to_owned();
                let kind = match args.get("kind").and_then(Value::as_str).unwrap_or("tmux") {
                    "tmux" => crate::mux::MuxKind::Tmux,
                    "zellij" => crate::mux::MuxKind::Zellij,
                    other => return Err((-32602, format!("unknown kind {other:?}"))),
                };
                (
                    IpcRequest::EndSession {
                        kind,
                        name,
                        host: args.get("host").and_then(Value::as_str).map(str::to_owned),
                    },
                    Duration::from_secs(70),
                )
            }
            "rename_session" => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| (-32602, String::from("`name` is required")))?
                    .to_owned();
                let new_name = args
                    .get("new_name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| (-32602, String::from("`new_name` is required")))?
                    .to_owned();
                let kind = match args.get("kind").and_then(Value::as_str).unwrap_or("tmux") {
                    "tmux" => crate::mux::MuxKind::Tmux,
                    "zellij" => crate::mux::MuxKind::Zellij,
                    other => return Err((-32602, format!("unknown kind {other:?}"))),
                };
                (
                    IpcRequest::RenameSession {
                        kind,
                        name,
                        new_name,
                        host: args.get("host").and_then(Value::as_str).map(str::to_owned),
                    },
                    Duration::from_secs(70),
                )
            }
            "close_pane" => (
                IpcRequest::ClosePane {
                    pane: opt_pane("pane"),
                },
                Duration::from_secs(70),
            ),
            "close_tab" => (
                IpcRequest::CloseTab {
                    window: opt_win("window"),
                    tab: args
                        .get("tab")
                        .and_then(Value::as_u64)
                        .map(crate::topology::TabId),
                },
                Duration::from_secs(70),
            ),
            "close_window" => (
                IpcRequest::CloseWindow {
                    window: opt_win("window"),
                },
                Duration::from_secs(70),
            ),
            "quit" => (IpcRequest::Quit, Duration::from_secs(70)),
            "list_sessions" => {
                let host = args.get("host").and_then(Value::as_str).map(str::to_owned);
                let refresh = args
                    .get("refresh")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // A named host can take seconds to answer, so allow more than
                // the local listing needs.
                let timeout = if host.is_some() {
                    Duration::from_secs(30)
                } else {
                    Duration::from_secs(15)
                };
                (
                    IpcRequest::ListSessions {
                        host,
                        refresh,
                        unattached_only: args
                            .get("unattached_only")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    },
                    timeout,
                )
            }
            "session_tree" => (
                IpcRequest::SessionTree {
                    refresh: args
                        .get("refresh")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                },
                Duration::from_secs(15),
            ),
            "open_session_grid" => {
                let sessions = args
                    .get("sessions")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let kind = match args.get("kind").and_then(Value::as_str) {
                    None => None,
                    Some("tmux") => Some(crate::mux::MuxKind::Tmux),
                    Some("zellij") => Some(crate::mux::MuxKind::Zellij),
                    Some(other) => {
                        return Err((-32602, format!("unknown kind {other:?}: tmux or zellij")));
                    }
                };
                (
                    IpcRequest::OpenSessionGrid {
                        sessions,
                        kind,
                        host: args.get("host").and_then(Value::as_str).map(str::to_owned),
                    },
                    Duration::from_secs(70),
                )
            }
            "handoff_to_peer" => (
                IpcRequest::HandoffToPeer {
                    pane: opt_pane("pane"),
                    peer: args.get("peer").and_then(Value::as_str).map(str::to_owned),
                    profile_id: args
                        .get("profile_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
                Duration::from_secs(70),
            ),
            "present_to_peer" => (
                IpcRequest::PresentToPeer {
                    peer: args.get("peer").and_then(Value::as_str).map(str::to_owned),
                    url: args.get("url").and_then(Value::as_str).map(str::to_owned),
                    path: args.get("path").and_then(Value::as_str).map(PathBuf::from),
                    text: args.get("text").and_then(Value::as_str).map(str::to_owned),
                    clipboard: args
                        .get("clipboard")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    profile_id: args
                        .get("profile_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    caption: args
                        .get("caption")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
                Duration::from_secs(70),
            ),
            "open_session" => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| (-32602, String::from("`name` is required")))?
                    .to_owned();
                let kind = match args.get("kind").and_then(Value::as_str).unwrap_or("tmux") {
                    "tmux" => crate::mux::MuxKind::Tmux,
                    "zellij" => crate::mux::MuxKind::Zellij,
                    other => {
                        return Err((-32602, format!("unknown kind {other:?}: tmux or zellij")));
                    }
                };
                let mode_text = args.get("mode").and_then(Value::as_str).unwrap_or("tab");
                let mode = crate::mux::SessionMode::parse(mode_text).ok_or_else(|| {
                    (
                        -32602,
                        format!("unknown mode {mode_text:?}: window, tab, split-right, split-down"),
                    )
                })?;
                (
                    IpcRequest::OpenSession {
                        kind,
                        name,
                        host: args.get("host").and_then(Value::as_str).map(str::to_owned),
                        mode,
                        window: opt_win("window"),
                        pane: opt_pane("pane"),
                    },
                    Duration::from_secs(30),
                )
            }
            "present_media" => {
                let path = args
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| (-32602, String::from("`path` is required")))?;
                let opt_u16 = |key: &str| args.get(key).and_then(Value::as_u64).map(|n| n as u16);
                (
                    IpcRequest::PresentMedia {
                        pane: opt_pane("pane"),
                        path: PathBuf::from(path),
                        cols: opt_u16("cols"),
                        rows: opt_u16("rows"),
                    },
                    Duration::from_secs(30),
                )
            }
            "present_notice" => {
                let message = args
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                (
                    IpcRequest::PresentNotice {
                        pane: opt_pane("pane"),
                        message,
                    },
                    Duration::from_secs(30),
                )
            }
            unknown => return Err((-32602, format!("unknown tool: {unknown}"))),
        };

        let resp = self.call_ipc(ipc_req, timeout)?;
        let is_error = matches!(resp, IpcResponse::Error { .. });
        let text = serde_json::to_string_pretty(&resp)
            .map_err(|e| (-32603, format!("serialize error: {e}")))?;

        Ok(json!({
            "content": [{
                "type": "text",
                "text": text
            }],
            "isError": is_error
        }))
    }
}

fn resource_metadata(uri: &str) -> (&'static str, &'static str) {
    match uri {
        "ui://mutiny/workspace" => (
            "Mutiny Workspace Topology",
            "Live windows, tabs, binary split trees, working directories, and pane attention flags",
        ),
        "ui://mutiny/actions" => (
            "Mutiny Action Catalog",
            "Every registered ActionSpec verb with its slug, shortcut, and consent tier; the slugs run_action accepts",
        ),
        "ui://mutiny/exited" => (
            "Recently Exited Panes",
            "Final screens and exit codes of the last 8 closed or failed panes",
        ),
        "ui://mutiny/audit" => (
            "Agent Audit Trail",
            "Redacted log of IPC and MCP calls with consent decisions and the carrier each arrived on",
        ),
        _ => ("Mutiny Resource", "Mutiny workspace state"),
    }
}

pub fn tool_definitions() -> Value {
    let mut tools = match json!([
        {
            "name": "list_workspace",
            "description": "Tier-1 Observe: list windows, tabs, split trees, working directories, prompt/output counters, and attention state.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_actions",
            "description": "Tier-1 Observe: every registered ActionSpec verb with slug, shortcut, consent tier, and summary. Use the slugs with run_action.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "read_screen_text",
            "description": "Tier-1 Observe: the visible viewport text of a pane, untrimmed, as a single string.",
            "inputSchema": {
                "type": "object",
                "properties": { "pane": { "type": "integer" } }
            }
        },
        {
            "name": "wait_prompt",
            "description": "Tier-1 Observe: block until the next OSC 133 prompt arrives on a pane. Use await_turn instead for raw-mode TUIs that emit no shell integration.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "integer" },
                    "since_seq": { "type": "integer" },
                    "timeout_ms": { "type": "integer" }
                }
            }
        },
        {
            "name": "read_pane_snapshot",
            "description": "Tier-1 Observe: structured terminal snapshot (dimensions, cursor, modes, OSC metadata, and trimmed screen lines).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "integer", "description": "Target pane id; omit for active pane" },
                    "max_lines": { "type": "integer", "description": "Optional trailing line cap" }
                }
            }
        },
        {
            "name": "read_last_command_output",
            "description": "Tier-1 Observe: read captured output and exit code of the last OSC 133 command executed in a pane.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "integer" }
                }
            }
        },
        {
            "name": "read_exited_pane",
            "description": "Tier-1 Observe: return final screen lines and exit code of a recently closed or fast-exited pane (8-slot ring).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "integer", "description": "Specific exited pane id, or omit for most recent exit" }
                }
            }
        },
        {
            "name": "search_scrollback",
            "description": "Tier-1 Observe: search a pane's visible and scrollback lines.",
            "inputSchema": {
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pane": { "type": "integer" },
                    "pattern": { "type": "string" },
                    "ignore_case": { "type": "boolean" },
                    "max_matches": { "type": "integer" }
                }
            }
        },
        {
            "name": "await_turn",
            "description": "Tier-1 Observe: wait until either OSC 133 prompt arrives or raw REPL output settles for settle_ms.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "integer" },
                    "since_prompt_seq": { "type": "integer" },
                    "since_output_seq": { "type": "integer" },
                    "settle_ms": { "type": "integer" },
                    "timeout_ms": { "type": "integer" }
                }
            }
        },
        {
            "name": "send_terminal_input",
            "description": "Tier-2 Act: send UTF-8 text (optionally bracketed-pasted) followed by named control keys (`enter`, `esc`, `ctrl-c`, arrows).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "integer" },
                    "text": { "type": "string" },
                    "keys": { "type": "array", "items": { "type": "string" } },
                    "bracketed_paste": { "type": "boolean" },
                    "return_snapshot": { "type": "boolean" }
                }
            }
        },
        {
            "name": "send_to_agent",
            "description": "Tier-2 Act: deliver a single submitted turn to a REPL or shell (bracketed paste when active + Enter) and return snapshot + sequence counters for await_turn.",
            "inputSchema": {
                "type": "object",
                "required": ["message"],
                "properties": {
                    "pane": { "type": "integer" },
                    "message": { "type": "string" },
                    "max_lines": { "type": "integer" }
                }
            }
        },
        {
            "name": "spawn_pane",
            "description": "Tier-2 Act: open a window, tab, or split (`window` | `tab` | `split-right` | `split-down`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "enum": ["window", "tab", "split-right", "split-down"] },
                    "pane": { "type": "integer" },
                    "window": { "type": "integer" },
                    "cwd": { "type": "string" }
                }
            }
        },
        {
            "name": "detach_split",
            "description": "Tier-2 Act: detach a split pane into its own top-level window without restarting its process.",
            "inputSchema": {
                "type": "object",
                "properties": { "pane": { "type": "integer" } }
            }
        },
        {
            "name": "move_pane",
            "description": "Tier-2 Act: move an existing pane into another tab.",
            "inputSchema": {
                "type": "object",
                "required": ["pane", "target_tab"],
                "properties": {
                    "pane": { "type": "integer" },
                    "target_tab": { "type": "integer" },
                    "down": { "type": "boolean" }
                }
            }
        },
        {
            "name": "focus_pane",
            "description": "Tier-2 Act: focus a pane across windows and tabs.",
            "inputSchema": {
                "type": "object",
                "required": ["pane"],
                "properties": { "pane": { "type": "integer" } }
            }
        },
        {
            "name": "run_action",
            "description": "Execute any registered ActionSpec verb by slug (same table as menu bar and command palette).",
            "inputSchema": {
                "type": "object",
                "required": ["slug"],
                "properties": {
                    "slug": { "type": "string" },
                    "window": { "type": "integer" }
                }
            }
        },
        {
            "name": "new_session",
            "description": "Tier-2 Act: create a NEW multiplexer session and open a pane on it. kind defaults to tmux; \"none\" opens a plain shell. Omit name to auto-name the smallest free mutiny-N. Fails if the name already exists, rather than attaching to it (use open_session for that).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["tmux", "zellij", "none"] },
                    "name": { "type": "string" },
                    "mode": { "type": "string", "enum": ["window", "tab", "split-right", "split-down"] },
                    "window": { "type": "integer" }
                }
            }
        },
        {
            "name": "propose_policy",
            "description": "Tier-1 Observe: propose a Tier-3 standing policy so a trusted agent stops prompting for every action. Grants NOTHING by itself: the operator installs it from Mutiny's Agent activity dialog. Name the verbs in scope (an empty scope is refused), optionally pin scalar arguments to exact values, and set a rate ceiling and expiry. Beyond the ceiling, calls fall back to per-action prompts. Returns policy_id; poll list_policies to see whether it was installed.",
            "inputSchema": {
                "type": "object",
                "required": ["description", "verbs"],
                "properties": {
                    "description": { "type": "string" },
                    "verbs": { "type": "array", "items": { "type": "string" } },
                    "arg_constraints": { "type": "object", "additionalProperties": { "type": "string" } },
                    "max_calls_per_minute": { "type": "integer" },
                    "expires_in_minutes": { "type": "integer" }
                }
            }
        },
        {
            "name": "list_policies",
            "description": "Tier-1 Observe: list installed and pending standing policies, with their scope, pins, rate ceiling, and expiry.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "install_policy",
            "description": "Tier-2 Act, operator-side: install a pending policy proposal. An agent arriving on an ask carrier cannot use this to install its own proposal, because the verb itself requires consent.",
            "inputSchema": {
                "type": "object",
                "required": ["id"],
                "properties": { "id": { "type": "integer" } }
            }
        },
        {
            "name": "revoke_policy",
            "description": "PolicyChange: revoke a standing policy. Takes effect immediately; subsequent calls fall back to per-action consent.",
            "inputSchema": {
                "type": "object",
                "required": ["id"],
                "properties": { "id": { "type": "integer" } }
            }
        }
    ]) {
        Value::Array(v) => v,
        _ => Vec::new(),
    };
    if let Value::Array(rest) = json!([
        {
            "name": "list_peers",
            "description": "Tier-1 Observe: list paired inbound HTTP MCP peers and paired outbound Haven limbs with live connection status, rate_limited_count, unread_alert_count, and signer enrollment metadata (redacted, never token hashes, bearer secrets, or signing_secret_b64).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_peer_alerts",
            "description": "Tier-1 Observe: list recent Haven limb transitions, stalled auth/consent alerts, inbound peer notices, and opt-in Android shade alerts.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "clear_peer_alerts",
            "description": "Tier-2 Act: mark all unread Haven limb and notification alerts as read and clear the !haven chrome badge.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "pair_haven",
            "description": "PolicyChange: pair an outbound Haven HTTP MCP limb over loopback or a WireGuard/Yggdrasil/private tunnel. Omit token to run symmetric initialize tap-to-pair. Always requires explicit operator confirmation on non-Operator carriers.",
            "inputSchema": {
                "type": "object",
                "required": ["label", "url"],
                "properties": {
                    "label": { "type": "string", "description": "Short human label for this Haven limb (for example pixel)" },
                    "url": { "type": "string", "description": "HTTP MCP URL over loopback or tunnel, e.g. http://127.0.0.1:8740/mcp" },
                    "token": { "type": "string", "description": "Optional pre-shared Bearer token; omit for interactive tap-to-pair" },
                    "enroll_signer": { "type": "boolean", "description": "Also provision Haven's HMAC-SHA256 consent signing secret right after pairing" }
                }
            }
        },
        {
            "name": "enroll_haven_signer",
            "description": "PolicyChange: call get_egress_signing_secret on a paired outbound Haven limb (with phone human confirmation) and persist its HMAC-SHA256 signing key in peers.json (0600) for cross-limb consent co-signing. Never covered by session grants or standing policies.",
            "inputSchema": {
                "type": "object",
                "required": ["id_or_label"],
                "properties": {
                    "id_or_label": { "type": "string", "description": "Outbound Haven peer id or label from list_peers" }
                }
            }
        },
        {
            "name": "revoke_peer",
            "description": "PolicyChange: revoke an inbound or outbound paired peer by id or label immediately in memory and in peers.json. Never covered by session grants or standing policies.",
            "inputSchema": {
                "type": "object",
                "required": ["id_or_label"],
                "properties": {
                    "id_or_label": { "type": "string", "description": "Peer id or label from list_peers" }
                }
            }
        },
        {
            "name": "end_session",
            "description": "Tier-4 Irreversible, DESTRUCTIVE: end a multiplexer session (tmux kill-session, zellij kill-session). Everything running inside it stops, on every device attached to it. This is NOT the same as close_tab or close_pane, which only end this client and leave the session running. Always requires per-action operator confirmation; never covered by session grants or standing policies.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" },
                    "kind": { "type": "string", "enum": ["tmux", "zellij"] },
                    "host": { "type": "string" }
                }
            }
        },
        {
            "name": "rename_session",
            "description": "Tier-2 Act: rename a multiplexer session using its own command (tmux rename-session, zellij action rename-session), then update every Mutiny pane attached to it so labels and the saved layout follow. Replies once the command finishes and reports non-zero exit errors.",
            "inputSchema": {
                "type": "object",
                "required": ["name", "new_name"],
                "properties": {
                    "name": { "type": "string" },
                    "new_name": { "type": "string" },
                    "kind": { "type": "string", "enum": ["tmux", "zellij"] },
                    "host": { "type": "string" }
                }
            }
        },
        {
            "name": "toggle_quick",
            "description": "Tier-2 Act: summon or dismiss the quick terminal, a scratch shell in its own window that keeps running while dismissed. Returns the window id and whether it is now showing. Summoning raises the window; it is not pinned above others, because GTK4 has no keep-above API.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "save_session",
            "description": "Tier-2 Act: write the current layout and opened session list to the state file now, instead of waiting for a clean exit. Use it to snapshot a layout deliberately.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "close_pane",
            "description": "Tier-2 Act: close a pane, defaulting to the active one. Closing the last pane of a tab closes the tab, and the last tab closes the window. For a pane attached to a multiplexer this kills only its client, never the server.",
            "inputSchema": {
                "type": "object",
                "properties": { "pane": { "type": "integer" } }
            }
        },
        {
            "name": "close_tab",
            "description": "Tier-2 Act: close a whole tab, defaulting to the active tab of the active window. Closing the last tab closes its window.",
            "inputSchema": {
                "type": "object",
                "properties": { "window": { "type": "integer" }, "tab": { "type": "integer" } }
            }
        },
        {
            "name": "close_window",
            "description": "Tier-2 Act: close a whole window and every pane in it, defaulting to the active window. Multiplexer clients die with their panes; the servers do not.",
            "inputSchema": {
                "type": "object",
                "properties": { "window": { "type": "integer" } }
            }
        },
        {
            "name": "quit",
            "description": "Tier-4 Irreversible: save the session, then quit Mutiny. This ends every mutiny-owned shell; processes inside tmux or zellij survive because the multiplexer owns them. Always requires per-action operator confirmation; never covered by session grants or standing policies.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_sessions",
            "description": "Tier-1 Observe locally, Tier-2 Act when a host is named: list tmux and zellij sessions. Omit host for THIS machine only. Naming a host authenticates to it over SSH with the operator's keys and runs a listing command there, which is why that case requires consent. Returns sessions plus per-host status, so an unavailable host is reported alongside the sessions that answered.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "SSH destination to scan. Omit for local only." },
                    "refresh": { "type": "boolean", "description": "Force a rescan instead of returning the cached inventory." },
                    "unattached_only": { "type": "boolean", "description": "Only tmux sessions with no client attached. A view for deliberate pruning, not an automatic cleanup: persistent detached sessions appear here too." }
                }
            }
        },
        {
            "name": "open_session",
            "description": "Tier-2 Act: attach a pane to an existing tmux or zellij session, in a new window, tab, or split. With host set, this runs the multiplexer ON THAT HOST over SSH. Attaches alongside other clients and never detaches them; creates the session if it does not exist. The multiplexer owns process survival, so closing the pane kills only its client.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string", "description": "Session name, exactly as listed by list_sessions." },
                    "kind": { "type": "string", "enum": ["tmux", "zellij"], "description": "Defaults to tmux." },
                    "host": { "type": "string", "description": "SSH destination. Omit for a local session." },
                    "mode": { "type": "string", "enum": ["window", "tab", "split-right", "split-down"], "description": "Defaults to tab." },
                    "window": { "type": "integer" },
                    "pane": { "type": "integer" }
                }
            }
        },
        {
            "name": "present_media",
            "description": "Tier-2 Act: read an image file and place it inline in a pane via Kitty graphics. Returns image_id and placement_id. cols/rows size it in cells; omit for natural size.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "pane": { "type": "integer" },
                    "path": { "type": "string", "description": "Absolute path to a PNG or JPEG on the mutiny host" },
                    "cols": { "type": "integer" },
                    "rows": { "type": "integer" }
                }
            }
        },
        {
            "name": "present_notice",
            "description": "Tier-2 Act: reach back to the human operator with an inline banner and urgent-next badge on a pane (pass empty/null message to clear).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "integer" },
                    "message": { "type": "string" }
                }
            }
        }
    ]) {
        tools.extend(rest);
    }
    if let Value::Array(rest2) = json!([
        {
            "name": "session_tree",
            "description": "Tier-1 Observe: hierarchical session tree across Local Sessions (nesting organs under the constellation root when present), SSH Hosts, and Paired Haven Limbs (live phone sessions, [agent] REPL indicators, and active bridges), with active/busy/!attention group rollups.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": { "type": "boolean", "description": "Trigger a fresh local scan and Haven limb inventory refresh before replying." }
                }
            }
        },
        {
            "name": "open_session_grid",
            "description": "Tier-2 Act: open a tiled multi-pane overview window attached to multiple multiplexer sessions (defaults to unattached or live local sessions up to 9 panes).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessions": { "type": "array", "items": { "type": "string" }, "description": "Explicit session names to tile (up to 9); omit to tile live local sessions." },
                    "kind": { "type": "string", "enum": ["tmux", "zellij"] },
                    "host": { "type": "string", "description": "Optional SSH host; omit for local sessions." }
                }
            }
        },
        {
            "name": "handoff_to_peer",
            "description": "Tier-2 Act: hand off a mux-backed pane (tmux/zellij) to a connected Haven limb. Focuses the tab immediately via focus_terminal_session if already connected on Haven, else connects via connect_profile with sessionName. Bare non-mux shells return an honest actionable error.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "integer", "description": "Target pane id; omit for the active pane." },
                    "peer": { "type": "string", "description": "Outbound Haven peer id or label; omit for the primary connected limb." },
                    "profile_id": { "type": "string", "description": "Haven SSH profile id or label; omit to match automatically from list_connections." }
                }
            }
        },
        {
            "name": "present_to_peer",
            "description": "Tier-2 Act: throw media (image/* or audio/* -> present_media), web/HTML/SVG/PDF (http(s) URL or .html/.svg/.pdf path -> present_web), or clipboard text (--text / selection -> write_clipboard) to a connected Haven limb.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer": { "type": "string", "description": "Outbound Haven peer id or label; omit for the primary connected limb." },
                    "url": { "type": "string", "description": "http:// or https:// URL to present on Haven." },
                    "path": { "type": "string", "description": "Absolute file path (.png/.jpg/.mp3 -> present_media, .html/.svg/.pdf -> present_web)." },
                    "text": { "type": "string", "description": "Plain text to write to Haven's clipboard." },
                    "clipboard": { "type": "boolean", "description": "Send the active pane's current selection to Haven's clipboard." },
                    "profile_id": { "type": "string", "description": "Haven backend profileId for path streaming (defaults to matching workstation SSH profile or 'local')." },
                    "caption": { "type": "string", "description": "Optional caption shown in Haven's bottom sheet." }
                }
            }
        }
    ]) {
        tools.extend(rest2);
    }
    Value::Array(tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::verb_samples;
    use std::io::Cursor;

    fn advertised_tool_names() -> Vec<String> {
        tool_definitions()
            .as_array()
            .expect("tool definitions are an array")
            .iter()
            .map(|t| t["name"].as_str().expect("tool has a name").to_owned())
            .collect()
    }

    #[test]
    fn every_ipc_verb_is_reachable_from_mcp() {
        let tools = advertised_tool_names();
        for sample in verb_samples() {
            let verb = sample.verb_name();
            let surface = mcp_surface_for(&sample)
                .unwrap_or_else(|| panic!("verb '{verb}' has no MCP surface"));
            match surface {
                McpSurface::Tool(name) => assert!(
                    tools.iter().any(|t| t == name),
                    "verb '{verb}' maps to tool '{name}', which tools/list does not advertise"
                ),
                McpSurface::Resource(uri) => assert!(
                    STATIC_RESOURCES.contains(&uri),
                    "verb '{verb}' maps to resource '{uri}', which resources/list does not list"
                ),
            }
        }
    }

    #[test]
    fn no_advertised_tool_is_dead_surface() {
        let reachable: Vec<&'static str> = verb_samples()
            .iter()
            .filter_map(|s| match mcp_surface_for(s) {
                Some(McpSurface::Tool(name)) => Some(name),
                _ => None,
            })
            .collect();
        for name in advertised_tool_names() {
            assert!(
                reachable.contains(&name.as_str()),
                "tool '{name}' is advertised but no IPC verb maps to it"
            );
        }
    }

    #[test]
    fn initialize_negotiates_instead_of_echoing() {
        let mut bridge = McpBridge::new(
            PathBuf::from("/nonexistent.sock"),
            ConsentMode::ObserveOnly,
            String::from("tester"),
        );
        let input = format!(
            "{}\n{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01","clientInfo":{"name":"peer"}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}),
        );
        let mut reader = Cursor::new(input.into_bytes());
        let mut out = Vec::new();
        bridge.serve(&mut reader, &mut out).unwrap();
        let replies: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            replies[0]["result"]["protocolVersion"],
            json!(DEFAULT_PROTOCOL_VERSION),
            "unsupported revision must fall back to ours, not echo"
        );
        assert_eq!(replies[1]["result"]["protocolVersion"], json!("2025-03-26"));
        assert_eq!(replies[0]["result"]["serverInfo"]["name"], json!("mutiny"));
    }

    #[test]
    fn actions_resource_is_listed_and_readable_without_a_workspace() {
        assert!(STATIC_RESOURCES.contains(&"ui://mutiny/actions"));
        let (name, desc) = resource_metadata("ui://mutiny/actions");
        assert!(name.contains("Action"));
        assert!(desc.contains("run_action"));
    }
}
