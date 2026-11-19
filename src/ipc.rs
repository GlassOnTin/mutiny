use std::env;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::action::{ACTIONS, ActionId, ConsentTier, spec};
use crate::engine::{CommandRecord, PaneSnapshot, ScrollbackMatch};
use crate::topology::{ExitedPaneRecord, PaneId, SplitAxis, TabId, WindowId, WorkspaceSummary};

/// Wire version of the JSONL envelope. Bump on any breaking shape change;
/// the server refuses envelopes that do not match.
pub const IPC_PROTOCOL_VERSION: u32 = 1;

fn default_ipc_version() -> u32 {
    IPC_PROTOCOL_VERSION
}

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

/// Caller-supplied label only. The consent mode is deliberately absent: it is
/// assigned by the carrier that accepted the connection, never by the client.
/// See `parse_envelope`, which refuses any envelope that tries to send one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CallerMeta {
    #[serde(default)]
    pub client: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcEnvelope {
    #[serde(default = "default_ipc_version")]
    pub version: u32,
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
    /// Mode the carrier assigned to this connection, recorded so the trail
    /// shows which trust path a call arrived on.
    pub carrier: ConsentMode,
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

/// The agent carrier sits beside the operator carrier. `mutiny mcp` connects
/// here, so the mode the GUI pinned for agents applies to it; `mutiny ctl`
/// connects to the operator socket and is the operator by virtue of being a
/// same-uid peer on that carrier.
pub fn agent_socket_path(operator_path: &Path) -> PathBuf {
    let stem = operator_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from("mutiny"));
    let name = match operator_path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}-agent.{ext}"),
        None => format!("{stem}-agent"),
    };
    operator_path.with_file_name(name)
}

/// Durable audit trail location. State, not config: it is written by the app
/// rather than edited by the user.
pub fn default_audit_path() -> PathBuf {
    let base = env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var("HOME")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".local").join("state"))
        })
        .unwrap_or_else(env::temp_dir);
    base.join("mutiny").join("audit.jsonl")
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
    parse_envelope(line.trim())
}

/// Decodes one envelope, enforcing the two carrier rules that a client must
/// not be able to talk its way past: it cannot claim a consent mode, and it
/// cannot speak a wire version the server does not implement.
pub fn parse_envelope(line: &str) -> io::Result<IpcEnvelope> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    if value.get("consent_mode").is_some() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "consent_mode is assigned by the transport and must not be sent by the client",
        ));
    }

    let envelope: IpcEnvelope =
        serde_json::from_value(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    if envelope.version != IPC_PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported IPC protocol version {} (server speaks {})",
                envelope.version, IPC_PROTOCOL_VERSION
            ),
        ));
    }
    Ok(envelope)
}

/// Reads the peer's uid/gid/pid from the kernel via `SO_PEERCRED`. This is the
/// only identity a Unix socket peer cannot forge: it is set by the kernel at
/// connect time, not by anything the client writes.
pub fn peer_credentials(stream: &UnixStream) -> io::Result<libc::ucred> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred)
}

/// A peer is admitted only when its kernel-reported uid matches the server's.
/// Split out from the socket call so the refusal path is testable without
/// needing a second user account.
pub fn carrier_permits_peer(cred: &libc::ucred, server_uid: u32) -> bool {
    cred.uid == server_uid
}

pub fn server_uid() -> u32 {
    unsafe { libc::getuid() }
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
            version: IPC_PROTOCOL_VERSION,
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

/// One canonical instance of every IPC verb, in `variant_ordinal` order. The
/// parity test drives off this list, so a verb missing here is a verb whose
/// MCP reachability is unasserted.
pub fn verb_samples() -> Vec<IpcRequest> {
    vec![
        IpcRequest::List,
        IpcRequest::Actions,
        IpcRequest::AuditLog { limit: 50 },
        IpcRequest::Snapshot {
            pane: None,
            max_lines: None,
        },
        IpcRequest::ReadExited { pane: None },
        IpcRequest::Search {
            pane: None,
            pattern: String::from("needle"),
            ignore_case: true,
            max_matches: 20,
        },
        IpcRequest::RunAction {
            action: ActionId::NewTab,
            window: None,
        },
        IpcRequest::SpawnWindow { cwd: None },
        IpcRequest::SpawnTab {
            window: None,
            cwd: None,
        },
        IpcRequest::Split {
            pane: None,
            axis: SplitAxis::Horizontal,
            cwd: None,
        },
        IpcRequest::Focus { pane: PaneId(1) },
        IpcRequest::DetachSplit { pane: None },
        IpcRequest::MovePane {
            pane: PaneId(1),
            target_tab: TabId(1),
            axis: SplitAxis::Horizontal,
        },
        IpcRequest::SendText {
            pane: None,
            text: String::from("x"),
        },
        IpcRequest::SendInput {
            pane: None,
            text: Some(String::from("x")),
            keys: vec![String::from("enter")],
            bracketed_paste: false,
            return_snapshot: false,
        },
        IpcRequest::SendToAgent {
            pane: None,
            message: String::from("x"),
            max_lines: None,
        },
        IpcRequest::PresentNotice {
            pane: None,
            message: Some(String::from("x")),
        },
        IpcRequest::LastOutput { pane: None },
        IpcRequest::ScreenText { pane: None },
        IpcRequest::WaitPrompt {
            pane: None,
            since_seq: None,
            timeout_ms: 1_000,
        },
        IpcRequest::AwaitTurn {
            pane: None,
            since_prompt_seq: None,
            since_output_seq: None,
            settle_ms: 250,
            timeout_ms: 1_000,
        },
    ]
}

/// Exhaustiveness lock for `verb_samples`. Adding an `IpcRequest` variant
/// fails compilation here, and the parity test then fails on the ordinal
/// bounds check until a sample is added. Together those two locks are what
/// make verb parity a property rather than an intention.
pub fn variant_ordinal(req: &IpcRequest) -> usize {
    match req {
        IpcRequest::List => 0,
        IpcRequest::Actions => 1,
        IpcRequest::AuditLog { .. } => 2,
        IpcRequest::Snapshot { .. } => 3,
        IpcRequest::ReadExited { .. } => 4,
        IpcRequest::Search { .. } => 5,
        IpcRequest::RunAction { .. } => 6,
        IpcRequest::SpawnWindow { .. } => 7,
        IpcRequest::SpawnTab { .. } => 8,
        IpcRequest::Split { .. } => 9,
        IpcRequest::Focus { .. } => 10,
        IpcRequest::DetachSplit { .. } => 11,
        IpcRequest::MovePane { .. } => 12,
        IpcRequest::SendText { .. } => 13,
        IpcRequest::SendInput { .. } => 14,
        IpcRequest::SendToAgent { .. } => 15,
        IpcRequest::PresentNotice { .. } => 16,
        IpcRequest::LastOutput { .. } => 17,
        IpcRequest::ScreenText { .. } => 18,
        IpcRequest::WaitPrompt { .. } => 19,
        IpcRequest::AwaitTurn { .. } => 20,
    }
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

    #[test]
    fn client_asserted_consent_mode_is_refused() {
        for mode in ["operator", "paired", "ask", "observe-only"] {
            let line = format!(r#"{{"op":"list","consent_mode":"{mode}"}}"#);
            let err = parse_envelope(&line).expect_err("must refuse a client-claimed mode");
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert!(
                err.to_string().contains("assigned by the transport"),
                "unhelpful refusal: {err}"
            );
        }
    }

    #[test]
    fn envelope_without_consent_mode_parses_and_defaults_version() {
        let env = parse_envelope(r#"{"op":"list","client":"ctl"}"#).expect("plain envelope");
        assert_eq!(env.version, IPC_PROTOCOL_VERSION);
        assert_eq!(env.meta.client.as_deref(), Some("ctl"));
        assert!(matches!(env.request, IpcRequest::List));
    }

    #[test]
    fn mismatched_protocol_version_is_refused() {
        let line = format!(r#"{{"version":{},"op":"list"}}"#, IPC_PROTOCOL_VERSION + 1);
        let err = parse_envelope(&line).expect_err("must refuse unknown version");
        assert!(err.to_string().contains("unsupported IPC protocol version"));
    }

    #[test]
    fn carrier_admits_only_matching_uid() {
        let ours = server_uid();
        let same = libc::ucred {
            pid: 1,
            uid: ours,
            gid: ours,
        };
        let other = libc::ucred {
            pid: 2,
            uid: ours.wrapping_add(1),
            gid: ours,
        };
        let root = libc::ucred {
            pid: 3,
            uid: 0,
            gid: 0,
        };
        assert!(carrier_permits_peer(&same, ours));
        assert!(!carrier_permits_peer(&other, ours));
        // Root is not exempt: a different uid is a different trust domain.
        assert_eq!(carrier_permits_peer(&root, ours), ours == 0);
    }

    #[test]
    fn live_socket_peer_credentials_match_server_uid() {
        let sock_path = env::temp_dir().join(format!("mutiny-cred-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = bind_socket(&sock_path).expect("bind");
        let client = UnixStream::connect(&sock_path).expect("connect");
        let (server_side, _) = listener.accept().expect("accept");
        let cred = peer_credentials(&server_side).expect("SO_PEERCRED");
        assert!(carrier_permits_peer(&cred, server_uid()));
        assert!(cred.pid > 0);
        drop(client);
        let _ = std::fs::remove_file(&sock_path);
    }

    #[test]
    fn verb_samples_cover_every_variant_exactly_once() {
        let samples = verb_samples();
        let mut seen = vec![false; samples.len()];
        for sample in &samples {
            let ord = variant_ordinal(sample);
            assert!(
                ord < seen.len(),
                "variant ordinal {ord} has no sample; extend verb_samples()"
            );
            assert!(!seen[ord], "duplicate sample for ordinal {ord}");
            seen[ord] = true;
        }
        assert!(seen.iter().all(|&s| s), "some variant has no sample");
    }

    #[test]
    fn agent_socket_path_sits_beside_operator_socket() {
        assert_eq!(
            agent_socket_path(Path::new("/run/user/1000/mutiny.sock")),
            PathBuf::from("/run/user/1000/mutiny-agent.sock")
        );
        assert_eq!(
            agent_socket_path(Path::new("/tmp/custom")),
            PathBuf::from("/tmp/custom-agent")
        );
    }
}
