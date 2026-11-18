use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::action::by_slug;
use crate::ipc::{CallerMeta, ConsentMode, IpcEnvelope, IpcRequest, IpcResponse, send_envelope};
use crate::topology::{PaneId, SplitAxis, TabId, WindowId};

const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

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
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let parsed: Result<JsonRpcRequest, _> = serde_json::from_str(trimmed);
            let req = match parsed {
                Ok(r) => r,
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
                    serde_json::to_writer(&mut *writer, &reply)?;
                    writer.write_all(b"\n")?;
                    writer.flush()?;
                    continue;
                }
            };

            let Some(id) = req.id.clone() else {
                // JSON-RPC notifications (e.g. notifications/initialized) produce no response.
                continue;
            };

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

            serde_json::to_writer(&mut *writer, &reply)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        Ok(())
    }

    fn call_ipc(
        &self,
        request: IpcRequest,
        timeout: Duration,
    ) -> Result<IpcResponse, (i32, String)> {
        let env_req = IpcEnvelope {
            meta: CallerMeta {
                client: Some(self.client_name.clone()),
                consent_mode: self.consent_mode,
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
                let requested_version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_PROTOCOL_VERSION);
                Ok(json!({
                    "protocolVersion": requested_version,
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
        let mut resources = vec![
            json!({
                "uri": "ui://mutiny/workspace",
                "name": "Mutiny Workspace Topology",
                "description": "Live windows, tabs, binary split trees, working directories, and pane attention flags",
                "mimeType": "application/json"
            }),
            json!({
                "uri": "ui://mutiny/exited",
                "name": "Recently Exited Panes",
                "description": "Final screens and exit codes of the last 8 closed or failed panes",
                "mimeType": "application/json"
            }),
            json!({
                "uri": "ui://mutiny/audit",
                "name": "Agent Audit Trail",
                "description": "Redacted log of IPC and MCP calls with consent decisions",
                "mimeType": "application/json"
            }),
        ];

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
                    Duration::from_secs(30),
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
                    Duration::from_secs(30),
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
                (req, Duration::from_secs(30))
            }
            "detach_split" => (
                IpcRequest::DetachSplit {
                    pane: opt_pane("pane"),
                },
                Duration::from_secs(30),
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
                    Duration::from_secs(30),
                )
            }
            "focus_pane" => {
                let pane =
                    opt_pane("pane").ok_or_else(|| (-32602, String::from("`pane` is required")))?;
                (IpcRequest::Focus { pane }, Duration::from_secs(30))
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

fn tool_definitions() -> Value {
    json!([
        {
            "name": "list_workspace",
            "description": "Tier-1 Observe: list windows, tabs, split trees, working directories, prompt/output counters, and attention state.",
            "inputSchema": { "type": "object", "properties": {} }
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
    ])
}
