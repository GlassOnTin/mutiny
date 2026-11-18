use std::env;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::action::{ACTIONS, ActionId, ConsentTier, spec};
use crate::engine::{CommandRecord, PaneSnapshot, ScrollbackMatch};
use crate::topology::{ExitedPaneRecord, PaneId, SplitAxis, TabId, WindowId, WorkspaceSummary};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsentMode {
    /// Local human CLI (`mutiny ctl`) — operator's own shell, audited as operator.
    #[default]
    Operator,
    /// Paired agent client — Tier-1 & Tier-2 allowed, visibly surfaced + audited.
    Paired,
    /// Interactive agent client — Tier-1 free, Tier-2 prompts in Mutiny window chrome.
    Ask,
    /// Strictly read-only agent client — Tier-2 verbs are rejected.
    ObserveOnly,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CallerMeta {
    #[serde(default)]
    pub client: Option<String>,
    #[serde(default)]
    pub consent_mode: ConsentMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcEnvelope {
    #[serde(flatten)]
    pub meta: CallerMeta,
    #[serde(flatten)]
    pub request: IpcRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum IpcRequest {
    List,
    Actions,
    AuditLog {
        #[serde(default = "default_audit_limit")]
        limit: usize,
    },
    Snapshot {
        pane: Option<PaneId>,
        max_lines: Option<usize>,
    },
    ReadExited {
        pane: Option<PaneId>,
    },
    Search {
        pane: Option<PaneId>,
        pattern: String,
        #[serde(default = "default_true")]
        ignore_case: bool,
        #[serde(default = "default_max_matches")]
        max_matches: usize,
    },
    RunAction {
        action: ActionId,
        window: Option<WindowId>,
    },
    SpawnWindow {
        cwd: Option<PathBuf>,
    },
    SpawnTab {
        window: Option<WindowId>,
        cwd: Option<PathBuf>,
    },
    Split {
        pane: Option<PaneId>,
        axis: SplitAxis,
        cwd: Option<PathBuf>,
    },
    Focus {
        pane: PaneId,
    },
    DetachSplit {
        pane: Option<PaneId>,
    },
    MovePane {
        pane: PaneId,
        target_tab: TabId,
        axis: SplitAxis,
    },
    SendText {
        pane: Option<PaneId>,
        text: String,
    },
    SendInput {
        pane: Option<PaneId>,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        keys: Vec<String>,
        #[serde(default)]
        bracketed_paste: bool,
        #[serde(default)]
        return_snapshot: bool,
    },
    SendToAgent {
        pane: Option<PaneId>,
        message: String,
        max_lines: Option<usize>,
    },
    PresentNotice {
        pane: Option<PaneId>,
        message: Option<String>,
    },
    LastOutput {
        pane: Option<PaneId>,
    },
    ScreenText {
        pane: Option<PaneId>,
    },
    WaitPrompt {
        pane: Option<PaneId>,
        since_seq: Option<u64>,
        timeout_ms: u64,
    },
    AwaitTurn {
        pane: Option<PaneId>,
        since_prompt_seq: Option<u64>,
        since_output_seq: Option<u64>,
        #[serde(default = "default_settle_ms")]
        settle_ms: u64,
        #[serde(default = "default_wait_timeout_ms")]
        timeout_ms: u64,
    },
}

fn default_true() -> bool {
    true
}
fn default_max_matches() -> usize {
    20
}
fn default_audit_limit() -> usize {
    50
}
fn default_settle_ms() -> u64 {
    250
}
fn default_wait_timeout_ms() -> u64 {
    30_000
}

impl IpcRequest {
    pub fn consent_tier(&self) -> ConsentTier {
        match self {
            IpcRequest::List
            | IpcRequest::Actions
            | IpcRequest::AuditLog { .. }
            | IpcRequest::Snapshot { .. }
            | IpcRequest::ReadExited { .. }
            | IpcRequest::Search { .. }
            | IpcRequest::LastOutput { .. }
            | IpcRequest::ScreenText { .. }
            | IpcRequest::WaitPrompt { .. }
            | IpcRequest::AwaitTurn { .. } => ConsentTier::Observe,

            IpcRequest::RunAction { action, .. } => spec(*action).consent_tier,

            IpcRequest::SpawnWindow { .. }
            | IpcRequest::SpawnTab { .. }
            | IpcRequest::Split { .. }
            | IpcRequest::Focus { .. }
            | IpcRequest::DetachSplit { .. }
            | IpcRequest::MovePane { .. }
            | IpcRequest::SendText { .. }
            | IpcRequest::SendInput { .. }
            | IpcRequest::SendToAgent { .. }
            | IpcRequest::PresentNotice { .. } => ConsentTier::Act,
        }
    }

    pub fn verb_name(&self) -> &'static str {
        match self {
            IpcRequest::List => "list",
            IpcRequest::Actions => "actions",
            IpcRequest::AuditLog { .. } => "audit-log",
            IpcRequest::Snapshot { .. } => "snapshot",
            IpcRequest::ReadExited { .. } => "read-exited",
            IpcRequest::Search { .. } => "search",
            IpcRequest::RunAction { .. } => "run-action",
            IpcRequest::SpawnWindow { .. } => "spawn-window",
            IpcRequest::SpawnTab { .. } => "spawn-tab",
            IpcRequest::Split { .. } => "split",
            IpcRequest::Focus { .. } => "focus",
            IpcRequest::DetachSplit { .. } => "detach-split",
            IpcRequest::MovePane { .. } => "move-pane",
            IpcRequest::SendText { .. } => "send-text",
            IpcRequest::SendInput { .. } => "send-input",
            IpcRequest::SendToAgent { .. } => "send-to-agent",
            IpcRequest::PresentNotice { .. } => "present-notice",
            IpcRequest::LastOutput { .. } => "last-output",
            IpcRequest::ScreenText { .. } => "screen-text",
            IpcRequest::WaitPrompt { .. } => "wait-prompt",
            IpcRequest::AwaitTurn { .. } => "await-turn",
        }
    }

    pub fn redacted_summary(&self) -> String {
        match self {
            IpcRequest::SendText { pane, text } => {
                format!("pane={pane:?} bytes={}", text.len())
            }
            IpcRequest::SendInput {
                pane,
                text,
                keys,
                bracketed_paste,
                ..
            } => format!(
                "pane={pane:?} bytes={} keys={keys:?} bracketed={bracketed_paste}",
                text.as_ref().map_or(0, String::len)
            ),
            IpcRequest::SendToAgent { pane, message, .. } => {
                format!("pane={pane:?} turn_bytes={}", message.len())
            }
            IpcRequest::PresentNotice { pane, message } => {
                let preview = message
                    .as_deref()
                    .map(|m| m.chars().take(48).collect::<String>())
                    .unwrap_or_else(|| String::from("<cleared>"));
                format!("pane={pane:?} notice={preview:?}")
            }
            IpcRequest::RunAction { action, window } => {
                format!("action={} window={window:?}", spec(*action).slug)
            }
            IpcRequest::Split { pane, axis, .. } => format!("pane={pane:?} axis={axis:?}"),
            IpcRequest::DetachSplit { pane } => format!("pane={pane:?}"),
            IpcRequest::MovePane {
                pane, target_tab, ..
            } => format!("pane={pane} target_tab={}", target_tab.0),
            IpcRequest::Focus { pane } => format!("pane={pane}"),
            IpcRequest::Search {
                pane,
                pattern,
                max_matches,
                ..
            } => format!(
                "pane={pane:?} pattern_len={} max={max_matches}",
                pattern.len()
            ),
            _ => String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuditDecision {
    Allowed,
    ApprovedOnce,
    ApprovedSession,
    Denied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub seq: u64,
    pub unix_ms: u64,
    pub client: String,
    pub verb: String,
    pub tier: ConsentTier,
    pub decision: AuditDecision,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionCatalogItem {
    pub slug: String,
    pub title: String,
    pub chord: String,
    pub tier: ConsentTier,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
#[allow(clippy::large_enum_variant)]
pub enum IpcResponse {
    Ok {
        #[serde(skip_serializing_if = "Option::is_none")]
        workspace: Option<WorkspaceSummary>,
        #[serde(skip_serializing_if = "Option::is_none")]
        actions: Option<Vec<ActionCatalogItem>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        window: Option<WindowId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab: Option<TabId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pane: Option<PaneId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        command: Option<CommandRecord>,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot: Option<PaneSnapshot>,
        #[serde(skip_serializing_if = "Option::is_none")]
        exited: Option<ExitedPaneRecord>,
        #[serde(skip_serializing_if = "Option::is_none")]
        exited_list: Option<Vec<ExitedPaneRecord>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        matches: Option<Vec<ScrollbackMatch>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        audit: Option<Vec<AuditEntry>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        bytes_sent: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt_seq: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output_seq: Option<u64>,
    },
    Error {
        message: String,
    },
}

impl IpcResponse {
    pub fn empty_ok() -> Self {
        IpcResponse::Ok {
            workspace: None,
            actions: None,
            window: None,
            tab: None,
            pane: None,
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
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        IpcResponse::Error {
            message: msg.into(),
        }
    }
}

pub fn default_socket_path() -> PathBuf {
    if let Ok(explicit) = env::var("MUTINY_SOCKET")
        && !explicit.is_empty()
    {
        return PathBuf::from(explicit);
    }
    if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR")
        && !runtime_dir.is_empty()
    {
        return PathBuf::from(runtime_dir).join("mutiny.sock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/mutiny-{uid}.sock"))
}

pub fn bind_socket(path: &Path) -> io::Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("live mutiny socket already listening at {}", path.display()),
            ));
        }
        let _ = std::fs::remove_file(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

pub fn read_envelope(stream: &mut UnixStream) -> io::Result<IpcEnvelope> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "empty IPC request",
        ));
    }
    serde_json::from_str(line.trim()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write_response(stream: &mut UnixStream, resp: &IpcResponse) -> io::Result<()> {
    let mut payload =
        serde_json::to_vec(resp).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    payload.push(b'\n');
    stream.write_all(&payload)?;
    stream.flush()
}

pub fn send_request(
    socket_path: &Path,
    req: &IpcRequest,
    read_timeout: Duration,
) -> io::Result<IpcResponse> {
    send_envelope(
        socket_path,
        &IpcEnvelope {
            meta: CallerMeta::default(),
            request: req.clone(),
        },
        read_timeout,
    )
}

pub fn send_envelope(
    socket_path: &Path,
    env_req: &IpcEnvelope,
    read_timeout: Duration,
) -> io::Result<IpcResponse> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(read_timeout))?;
    let mut payload =
        serde_json::to_vec(env_req).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    payload.push(b'\n');
    stream.write_all(&payload)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "server closed IPC socket without replying",
        ));
    }
    serde_json::from_str(line.trim()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn action_catalog() -> Vec<ActionCatalogItem> {
    ACTIONS
        .iter()
        .map(|a| ActionCatalogItem {
            slug: a.slug.to_owned(),
            title: a.title.to_owned(),
            chord: a.human_chord.to_owned(),
            tier: a.consent_tier,
            summary: a.summary.to_owned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn round_trips_ipc_over_unix_socket_and_rejects_live_collision() {
        let sock_path = env::temp_dir().join(format!("mutiny-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);

        let listener = bind_socket(&sock_path).expect("bind socket");
        listener.set_nonblocking(false).unwrap();

        let sock_for_server = sock_path.clone();
        let server = thread::spawn(move || {
            let collision = bind_socket(&sock_for_server);
            assert!(collision.is_err());

            let (mut stream, _) = listener.accept().expect("accept client");
            let env_req = read_envelope(&mut stream).expect("decode request");
            assert!(matches!(env_req.request, IpcRequest::Actions));
            let mut resp = IpcResponse::empty_ok();
            if let IpcResponse::Ok { actions, .. } = &mut resp {
                *actions = Some(action_catalog());
            }
            write_response(&mut stream, &resp).expect("write response");
        });

        let resp = send_request(&sock_path, &IpcRequest::Actions, Duration::from_secs(3))
            .expect("client send_request");
        match resp {
            IpcResponse::Ok {
                actions: Some(items),
                ..
            } => {
                assert_eq!(items.len(), ACTIONS.len());
                assert!(items.iter().any(|i| i.slug == "detach-split"));
            }
            other => panic!("unexpected response: {other:?}"),
        }

        server.join().unwrap();
        let _ = std::fs::remove_file(&sock_path);
    }
}
