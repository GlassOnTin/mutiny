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

/// Trust domain of the carrier that accepted a call. Every remote HTTP peer
/// (CLI `--http-peer` or durable `peers.json`) resolves strictly to `Peer`,
/// never `Operator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CarrierRole {
    Operator,
    Peer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsentMode {
    /// Operator carrier: a same-uid peer on the operator socket, audited as
    /// the operator. The human running `mutiny ctl` is the consent.
    #[default]
    Operator,
    /// Agent carrier pinned to paired: Tier-1 and Tier-2 allowed, surfaced
    /// and audited. An operator-side grant, never a client request.
    Paired,
    /// Agent carrier pinned to ask: Tier-1 free, Tier-2 prompts in window
    /// chrome unless this peer identity holds a session grant.
    Ask,
    /// Agent carrier pinned to observe-only: Tier-2 verbs are rejected.
    ObserveOnly,
}

impl ConsentMode {
    pub const fn carrier_role(self) -> CarrierRole {
        match self {
            Self::Operator => CarrierRole::Operator,
            Self::Paired | Self::Ask | Self::ObserveOnly => CarrierRole::Peer,
        }
    }

    /// Parses a mode an operator may assign to an agent or network peer.
    ///
    /// `Operator` is deliberately unparseable here. It belongs to the operator
    /// socket, where a same-uid human is the consent; no configuration string
    /// and no client can reach it. Keeping the refusal at the type's own parse
    /// boundary means a new caller cannot forget the check.
    pub fn parse_peer_mode(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "ask" => Ok(Self::Ask),
            "paired" => Ok(Self::Paired),
            "observe-only" => Ok(Self::ObserveOnly),
            "operator" => Err(String::from(
                "operator is not assignable to a peer; it belongs to the operator socket",
            )),
            other => Err(format!(
                "unknown consent mode {other:?}: expected ask, paired, or observe-only"
            )),
        }
    }
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
    /// Creates a NEW multiplexer session and opens a pane on it. `kind: None`
    /// means a plain shell; `name: None` auto-names the smallest free mutiny-N.
    /// Unlike OpenSession this fails on a name collision instead of attaching.
    NewSession {
        kind: Option<crate::mux::MuxKind>,
        name: Option<String>,
        mode: crate::mux::SessionMode,
        window: Option<WindowId>,
    },
    /// Proposes a Tier-3 standing policy. Grants NOTHING by itself: it queues a
    /// proposal the operator installs from the Agent activity dialog. Observe
    /// tier, because asking is not acting.
    ProposePolicy {
        description: String,
        verbs: Vec<String>,
        #[serde(default)]
        arg_constraints: std::collections::BTreeMap<String, String>,
        max_calls_per_minute: u32,
        expires_in_minutes: u64,
    },
    /// Lists installed and pending standing policies.
    ListPolicies,
    /// Installs a pending proposal. Operator-side; an agent cannot install its
    /// own proposal.
    InstallPolicy {
        id: u64,
    },
    /// Revokes an installed policy.
    RevokePolicy {
        id: u64,
    },
    /// Lists inbound paired peers (never token material or hashes) and outbound
    /// Haven limbs with live connection status.
    ListPeers,
    /// Pairs an outbound Haven HTTP MCP limb over loopback or an encrypted
    /// tunnel, optionally running symmetric `initialize` tap-to-pair when
    /// `token` is omitted.
    PairHaven {
        label: String,
        url: String,
        #[serde(default)]
        token: Option<String>,
    },
    /// Revokes an inbound or outbound paired peer immediately in memory and in
    /// `peers.json`.
    RevokePeer {
        id_or_label: String,
    },
    /// ENDS a multiplexer session. Destructive: everything running inside it
    /// stops, on every device. Distinct from closing a tab, which only ends
    /// this client and leaves the session running. Tier-4 Irreversible: never
    /// covered by a session grant or a standing policy.
    EndSession {
        kind: crate::mux::MuxKind,
        name: String,
        host: Option<String>,
    },
    /// Renames a multiplexer session using its own command, then updates every
    /// pane attached to it.
    RenameSession {
        kind: crate::mux::MuxKind,
        name: String,
        new_name: String,
        host: Option<String>,
    },
    /// Writes the current layout and opened session list to the state file now,
    /// without waiting for a clean exit. Lets a layout be snapshotted on demand.
    SaveSession,
    /// Summon or dismiss the quick terminal.
    ToggleQuick,
    /// Closes a pane. If it is the last pane of a tab the tab goes, and if that
    /// was the last tab the window goes. The symmetric counterpart to the spawn
    /// verbs, which previously had no matching close.
    ClosePane {
        pane: Option<PaneId>,
    },
    /// Closes a whole tab. Defaults to the active tab of the active window.
    CloseTab {
        window: Option<WindowId>,
        tab: Option<TabId>,
    },
    /// Closes a whole window and every pane in it. Defaults to the active one.
    CloseWindow {
        window: Option<WindowId>,
    },
    /// Saves the session and quits. Tier-4 Irreversible: it ends every
    /// mutiny-owned shell and always requires explicit per-action confirmation.
    Quit,
    /// Lists tmux/zellij sessions. `host: None` means THIS machine only, which
    /// is Observe. Naming a host authenticates to it with the user's keys and
    /// runs a command there, so that case is Act: an observe-only carrier must
    /// not get lateral movement for free.
    ListSessions {
        host: Option<String>,
        #[serde(default)]
        refresh: bool,
        /// Only sessions with no client attached. These are the ones a New Tab
        /// leaves behind, but also the persistent ones an operator deliberately
        /// detached, so this is a VIEW for deliberate pruning, never an
        /// automatic cleanup.
        #[serde(default)]
        unattached_only: bool,
    },
    /// Attaches a pane to a multiplexer session, locally or over SSH. The
    /// multiplexer owns process survival; closing the pane kills only its
    /// client.
    OpenSession {
        kind: crate::mux::MuxKind,
        name: String,
        host: Option<String>,
        mode: crate::mux::SessionMode,
        window: Option<WindowId>,
        pane: Option<PaneId>,
    },
    /// Reads an image file and places it inline in a pane via Kitty graphics.
    /// Additive verb; existing request and response shapes are unchanged.
    PresentMedia {
        pane: Option<PaneId>,
        path: PathBuf,
        cols: Option<u16>,
        rows: Option<u16>,
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

            // Local listing is observation. Naming a remote host spends the
            // user's SSH credentials and runs a command there, which is not.
            IpcRequest::ListSessions { host, .. } => {
                if host.is_some() {
                    ConsentTier::Act
                } else {
                    ConsentTier::Observe
                }
            }
            IpcRequest::OpenSession { .. } => ConsentTier::Act,
            IpcRequest::SaveSession => ConsentTier::Act,
            IpcRequest::ToggleQuick => ConsentTier::Act,
            IpcRequest::NewSession { .. } => ConsentTier::Act,
            IpcRequest::RenameSession { .. } => ConsentTier::Act,
            IpcRequest::ProposePolicy { .. } => ConsentTier::Observe,
            IpcRequest::ListPolicies => ConsentTier::Observe,
            IpcRequest::InstallPolicy { .. } => ConsentTier::PolicyChange,
            IpcRequest::RevokePolicy { .. } => ConsentTier::PolicyChange,
            IpcRequest::ListPeers => ConsentTier::Observe,
            IpcRequest::PairHaven { .. } => ConsentTier::PolicyChange,
            IpcRequest::RevokePeer { .. } => ConsentTier::PolicyChange,
            IpcRequest::EndSession { .. } => ConsentTier::Irreversible,
            IpcRequest::ClosePane { .. } => ConsentTier::Act,
            IpcRequest::CloseTab { .. } => ConsentTier::Act,
            IpcRequest::CloseWindow { .. } => ConsentTier::Act,
            IpcRequest::Quit => ConsentTier::Irreversible,

            IpcRequest::SpawnWindow { .. }
            | IpcRequest::SpawnTab { .. }
            | IpcRequest::Split { .. }
            | IpcRequest::Focus { .. }
            | IpcRequest::DetachSplit { .. }
            | IpcRequest::MovePane { .. }
            | IpcRequest::SendText { .. }
            | IpcRequest::SendInput { .. }
            | IpcRequest::SendToAgent { .. }
            | IpcRequest::PresentNotice { .. }
            | IpcRequest::PresentMedia { .. } => ConsentTier::Act,
        }
    }

    /// Scalar fields a standing policy can pin, as exact-match strings. Only
    /// the verbs where pinning is meaningful carry entries; everything else
    /// returns an empty map, so a policy pinning a key the call lacks does not
    /// match rather than matching vacuously.
    pub fn pin_fields(&self) -> std::collections::BTreeMap<String, String> {
        let mut m = std::collections::BTreeMap::new();
        let mut put = |k: &str, v: String| {
            m.insert(k.to_owned(), v);
        };
        match self {
            IpcRequest::OpenSession {
                kind,
                host,
                mode,
                window,
                ..
            } => {
                put("kind", kind.label().to_owned());
                put("host", host.clone().unwrap_or_default());
                put("mode", format!("{mode:?}").to_lowercase());
                window.map(|w| put("window", w.0.to_string()));
            }
            IpcRequest::NewSession {
                kind, mode, window, ..
            } => {
                put("kind", kind.map(|k| k.label()).unwrap_or("none").to_owned());
                put("mode", format!("{mode:?}").to_lowercase());
                window.map(|w| put("window", w.0.to_string()));
            }
            IpcRequest::SendInput { pane, .. } | IpcRequest::SendText { pane, .. } => {
                pane.map(|p| put("pane", p.0.to_string()));
            }
            IpcRequest::SpawnTab { window, .. } => {
                window.map(|w| put("window", w.0.to_string()));
            }
            IpcRequest::Split { pane, axis, .. } => {
                put("axis", format!("{axis:?}").to_lowercase());
                pane.map(|p| put("pane", p.0.to_string()));
            }
            IpcRequest::RunAction { action, .. } => {
                put("action", crate::action::spec(*action).slug.to_owned());
            }
            _ => {}
        }
        m
    }

    pub fn verb_name(&self) -> &'static str {
        match self {
            IpcRequest::List => "list",
            IpcRequest::ListSessions { .. } => "list-sessions",
            IpcRequest::OpenSession { .. } => "open-session",
            IpcRequest::SaveSession => "save-session",
            IpcRequest::ToggleQuick => "toggle-quick",
            IpcRequest::NewSession { .. } => "new-session",
            IpcRequest::RenameSession { .. } => "rename-session",
            IpcRequest::ProposePolicy { .. } => "propose-policy",
            IpcRequest::ListPolicies => "list-policies",
            IpcRequest::InstallPolicy { .. } => "install-policy",
            IpcRequest::RevokePolicy { .. } => "revoke-policy",
            IpcRequest::ListPeers => "list-peers",
            IpcRequest::PairHaven { .. } => "pair-haven",
            IpcRequest::RevokePeer { .. } => "revoke-peer",
            IpcRequest::EndSession { .. } => "end-session",
            IpcRequest::ClosePane { .. } => "close-pane",
            IpcRequest::CloseTab { .. } => "close-tab",
            IpcRequest::CloseWindow { .. } => "close-window",
            IpcRequest::Quit => "quit",
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
            IpcRequest::PresentMedia { .. } => "present-media",
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
            IpcRequest::ListSessions {
                host,
                refresh,
                unattached_only,
            } => {
                format!("host={host:?} refresh={refresh} unattached_only={unattached_only}")
            }
            IpcRequest::ClosePane { pane } => format!("pane={pane:?}"),
            IpcRequest::NewSession {
                kind, name, mode, ..
            } => {
                format!("kind={kind:?} name={name:?} mode={mode:?}")
            }
            IpcRequest::RenameSession {
                kind,
                name,
                new_name,
                host,
            } => format!(
                "kind={kind:?} host={host:?} name={:?} new_name={:?}",
                crate::mux::sanitize_text(name, 64),
                crate::mux::sanitize_text(new_name, 64)
            ),
            IpcRequest::CloseTab { window, tab } => format!("window={window:?} tab={tab:?}"),
            IpcRequest::CloseWindow { window } => format!("window={window:?}"),
            IpcRequest::ProposePolicy {
                description,
                verbs,
                arg_constraints,
                max_calls_per_minute,
                expires_in_minutes,
            } => {
                let clean_verbs: Vec<String> = verbs
                    .iter()
                    .take(8)
                    .map(|v| crate::mux::sanitize_text(v, 32))
                    .collect();
                format!(
                    "desc={:?} verbs={clean_verbs:?} pins={} max_per_min={max_calls_per_minute} expires_min={expires_in_minutes}",
                    crate::mux::sanitize_text(description, 48),
                    arg_constraints.len()
                )
            }
            IpcRequest::InstallPolicy { id } | IpcRequest::RevokePolicy { id } => {
                format!("id={id}")
            }
            IpcRequest::PairHaven { label, url, token } => format!(
                "label={:?} url={:?} has_token={}",
                crate::mux::sanitize_text(label, 48),
                crate::mux::sanitize_text(url, 96),
                token.is_some()
            ),
            IpcRequest::RevokePeer { id_or_label } => {
                format!("target={:?}", crate::mux::sanitize_text(id_or_label, 64))
            }
            IpcRequest::EndSession { kind, name, host } => format!(
                "kind={kind:?} host={host:?} name={:?}",
                crate::mux::sanitize_text(name, 64)
            ),
            IpcRequest::OpenSession {
                kind,
                name,
                host,
                mode,
                ..
            } => format!(
                "kind={kind:?} host={host:?} mode={mode:?} name={:?}",
                crate::mux::sanitize_text(name, 64)
            ),
            IpcRequest::PresentMedia {
                pane,
                path,
                cols,
                rows,
            } => format!(
                "pane={pane:?} path={:?} cols={cols:?} rows={rows:?}",
                path.display()
            ),
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
    /// Allowed by an installed Tier-3 standing policy rather than a prompt.
    ApprovedPolicy,
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
    /// Kernel-verified peer the call came from. `client` is the label that
    /// peer chose; this is the process it actually was. Defaults to a zero
    /// identity so entries written before this field existed still load
    /// rather than being dropped from the trail. A zero pid is never a live
    /// peer, so a legacy entry can never match a grant.
    #[serde(default)]
    pub peer: PeerIdentity,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        image_id: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        placement_id: Option<u32>,
        /// Multiplexer session inventory, with per-host status so a failed host
        /// is reported alongside the sessions that did answer.
        #[serde(skip_serializing_if = "Option::is_none")]
        sessions: Option<crate::mux::MuxInventory>,
        #[serde(skip_serializing_if = "Option::is_none")]
        policy_id: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        policies: Option<Vec<crate::policy::StandingPolicy>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pending_policies: Option<Vec<crate::policy::StandingPolicy>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        peers: Option<crate::peers::PeersListing>,
        #[serde(skip_serializing_if = "Option::is_none")]
        outbound_peer: Option<crate::peers::OutboundPeerSummary>,
        /// Whether a toggled window is now showing rather than dismissed.
        #[serde(skip_serializing_if = "Option::is_none")]
        visible: Option<bool>,
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
            image_id: None,
            placement_id: None,
            sessions: None,
            policy_id: None,
            policies: None,
            pending_policies: None,
            peers: None,
            outbound_peer: None,
            visible: None,
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

/// Kernel-verified identity of a socket peer. Session grants key on this, never
/// on the `client` label, which is display text a caller chooses.
///
/// `start_time` is the process start time from `/proc/<pid>/stat`, included so a
/// recycled pid does not inherit a dead process's grant. It reads as 0 when
/// `/proc` is unavailable, which degrades pid-reuse protection but still keeps
/// distinct live processes distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct PeerIdentity {
    pub uid: u32,
    pub pid: i32,
    pub start_time: u64,
    /// Set when the peer's credential is a pairing token rather than a kernel
    /// uid/pid, as on the HTTP carrier. HTTP has no `SO_PEERCRED` equivalent, so
    /// the token is the credential and this fingerprint is the identity derived
    /// from it. `pid` stays 0, which is never a live process, so a token peer can
    /// neither collide with nor inherit a Unix process's grant.
    ///
    /// Absent for kernel-verified peers and for entries written before this
    /// field existed, so the trail stays loadable and unchanged for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_peer: Option<u64>,
}

impl PeerIdentity {
    pub fn from_credentials(cred: &libc::ucred) -> Self {
        Self {
            uid: cred.uid,
            pid: cred.pid,
            start_time: process_start_time(cred.pid),
            token_peer: None,
        }
    }

    /// Identity for a peer authenticated by a pairing token. Assigned by the
    /// carrier that verified the token, never chosen by the client.
    pub fn for_token(token: &str, uid: u32) -> Self {
        Self {
            uid,
            pid: 0,
            start_time: 0,
            token_peer: Some(token_fingerprint(token)),
        }
    }

    /// Identity for a durable peer stored in `peers.json`, where only the
    /// SHA-256 hex digest of the token exists at rest.
    pub fn for_token_sha256(token_sha256_hex: &str, uid: u32) -> Self {
        let fp = match crate::crypto::hex_decode_32(token_sha256_hex) {
            Ok(digest) => u64::from_be_bytes([
                digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6],
                digest[7],
            ]),
            Err(_) => token_fingerprint(&token_sha256_hex.to_ascii_lowercase()),
        };
        Self {
            uid,
            pid: 0,
            start_time: 0,
            token_peer: Some(fp),
        }
    }

    /// True when this identity came from a pairing token rather than the kernel.
    pub fn is_token_peer(&self) -> bool {
        self.token_peer.is_some()
    }
}

/// Stable 64-bit FNV-1a over the token bytes.
///
/// Not a cryptographic hash, deliberately: the fingerprint is an identifier,
/// not a secret, and it is only ever computed from a token the carrier has
/// already accepted by exact constant-time comparison. An attacker cannot
/// present an arbitrary token to aim at a collision, so collision resistance
/// buys nothing here. FNV-1a is chosen because it is tiny, dependency-free, and
/// fixed for all time, which matters for a durable trail: identities must stay
/// comparable across restarts and compiler versions.
pub fn token_fingerprint(token: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in token.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Field 22 of `/proc/<pid>/stat`: clock ticks since boot at process start.
/// The comm field can contain spaces and parentheses, so parse after the last
/// ')' rather than splitting the whole line.
pub fn process_start_time(pid: i32) -> u64 {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return 0;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return 0;
    };
    // rest begins at field 3 (state), so field 22 is index 19.
    rest.split_whitespace()
        .nth(19)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// What the carrier's trust rules decide for one call. Pure so the policy can
/// be tested without a GUI, a socket, or a second user account.
///
/// The client label is deliberately not an input: a grant cannot follow a
/// claimed name because the name never reaches this decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsentOutcome {
    Allow(AuditDecision),
    Deny(String),
    /// Park the stream and raise the in-window consent bar.
    Park,
}

pub fn consent_outcome(
    tier: ConsentTier,
    carrier: ConsentMode,
    identity: PeerIdentity,
    grants: &std::collections::HashSet<PeerIdentity>,
    gui_present: bool,
) -> ConsentOutcome {
    match (tier, carrier) {
        (ConsentTier::Observe, _) => ConsentOutcome::Allow(AuditDecision::Allowed),
        (ConsentTier::Act, ConsentMode::Operator | ConsentMode::Paired) => {
            ConsentOutcome::Allow(AuditDecision::Allowed)
        }
        (ConsentTier::Act, ConsentMode::ObserveOnly) => ConsentOutcome::Deny(String::from(
            "observe-only session cannot execute Tier-2 Act verbs",
        )),
        (ConsentTier::Act, ConsentMode::Ask) => {
            if grants.contains(&identity) {
                ConsentOutcome::Allow(AuditDecision::ApprovedSession)
            } else if !gui_present {
                // Fail closed when no interactive window can present consent.
                ConsentOutcome::Deny(String::from(
                    "consent prompt requires an open Mutiny GUI window",
                ))
            } else {
                ConsentOutcome::Park
            }
        }
        // PolicyChange: mutates trust boundaries (standing policies or paired
        // peers). Never auto-approved by session grants, standing policies, or
        // paired agent carriers; only the operator carrier is self-consenting.
        (ConsentTier::PolicyChange, ConsentMode::Operator) => {
            ConsentOutcome::Allow(AuditDecision::Allowed)
        }
        (ConsentTier::PolicyChange, ConsentMode::ObserveOnly) => ConsentOutcome::Deny(
            String::from("observe-only session cannot execute PolicyChange verbs"),
        ),
        (ConsentTier::PolicyChange, ConsentMode::Ask | ConsentMode::Paired) => {
            if !gui_present {
                ConsentOutcome::Deny(String::from(
                    "consent prompt requires an open Mutiny GUI window",
                ))
            } else {
                ConsentOutcome::Park
            }
        }
        // Tier 4: destroys state the operator cannot recover. Always per-action,
        // never standing: neither a session grant, a standing policy, nor a
        // paired agent carrier bypasses explicit confirmation. Only the human
        // running `mutiny ctl` on the operator socket is the consent.
        (ConsentTier::Irreversible, ConsentMode::Operator) => {
            ConsentOutcome::Allow(AuditDecision::Allowed)
        }
        (ConsentTier::Irreversible, ConsentMode::ObserveOnly) => ConsentOutcome::Deny(
            String::from("observe-only session cannot execute Tier-4 Irreversible verbs"),
        ),
        (ConsentTier::Irreversible, ConsentMode::Ask | ConsentMode::Paired) => {
            if !gui_present {
                ConsentOutcome::Deny(String::from(
                    "consent prompt requires an open Mutiny GUI window",
                ))
            } else {
                ConsentOutcome::Park
            }
        }
    }
}

/// Classification of a verb slug for Tier-3 standing-policy scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyVerbScope {
    Allowed,
    Irreversible,
    PolicyManagement,
    Unknown,
}

/// Checks whether a verb slug may appear in a Tier-3 standing policy. Policy
/// and peer management verbs and Tier-4 irreversible verbs are never coverable;
/// membership is derived from `verb_samples()` so newly added `PolicyChange` or
/// `Irreversible` verbs cannot be forgotten here.
pub fn classify_policy_verb(verb: &str) -> PolicyVerbScope {
    if matches!(verb, "propose-policy" | "list-policies" | "list-peers") {
        return PolicyVerbScope::PolicyManagement;
    }
    for sample in verb_samples() {
        if sample.verb_name() == verb {
            return match sample.consent_tier() {
                ConsentTier::Irreversible => PolicyVerbScope::Irreversible,
                ConsentTier::PolicyChange => PolicyVerbScope::PolicyManagement,
                ConsentTier::Observe | ConsentTier::Act => PolicyVerbScope::Allowed,
            };
        }
    }
    PolicyVerbScope::Unknown
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
        IpcRequest::PresentMedia {
            pane: None,
            path: PathBuf::from("/nonexistent.png"),
            cols: None,
            rows: None,
        },
        IpcRequest::ListSessions {
            host: None,
            refresh: false,
            unattached_only: false,
        },
        IpcRequest::OpenSession {
            kind: crate::mux::MuxKind::Tmux,
            name: String::from("mutiny-probe"),
            host: None,
            mode: crate::mux::SessionMode::Tab,
            window: None,
            pane: None,
        },
        IpcRequest::SaveSession,
        IpcRequest::ToggleQuick,
        IpcRequest::NewSession {
            kind: Some(crate::mux::MuxKind::Tmux),
            name: Some(String::from("mutiny-probe")),
            mode: crate::mux::SessionMode::Tab,
            window: None,
        },
        IpcRequest::RenameSession {
            kind: crate::mux::MuxKind::Tmux,
            name: String::from("mutiny-probe"),
            new_name: String::from("mutiny-probe-renamed"),
            host: None,
        },
        IpcRequest::EndSession {
            kind: crate::mux::MuxKind::Tmux,
            name: String::from("mutiny-probe-absent"),
            host: None,
        },
        IpcRequest::ProposePolicy {
            description: String::from("probe policy"),
            verbs: vec![String::from("send-input")],
            arg_constraints: std::collections::BTreeMap::new(),
            max_calls_per_minute: 10,
            expires_in_minutes: 5,
        },
        IpcRequest::ListPolicies,
        IpcRequest::InstallPolicy { id: 999_999 },
        IpcRequest::RevokePolicy { id: 999_999 },
        IpcRequest::ClosePane { pane: None },
        IpcRequest::CloseTab {
            window: None,
            tab: None,
        },
        IpcRequest::CloseWindow { window: None },
        IpcRequest::Quit,
        IpcRequest::ListPeers,
        IpcRequest::PairHaven {
            label: String::from("haven-probe"),
            url: String::from("http://127.0.0.1:1/mcp"),
            token: Some(String::from("probe")),
        },
        IpcRequest::RevokePeer {
            id_or_label: String::from("absent-peer"),
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
        IpcRequest::PresentMedia { .. } => 21,
        IpcRequest::ListSessions { .. } => 22,
        IpcRequest::OpenSession { .. } => 23,
        IpcRequest::SaveSession => 24,
        IpcRequest::ToggleQuick => 36,
        IpcRequest::NewSession { .. } => 29,
        IpcRequest::RenameSession { .. } => 30,
        IpcRequest::ProposePolicy { .. } => 32,
        IpcRequest::ListPolicies => 33,
        IpcRequest::InstallPolicy { .. } => 34,
        IpcRequest::RevokePolicy { .. } => 35,
        IpcRequest::ListPeers => 37,
        IpcRequest::PairHaven { .. } => 38,
        IpcRequest::RevokePeer { .. } => 39,
        IpcRequest::EndSession { .. } => 31,
        IpcRequest::ClosePane { .. } => 25,
        IpcRequest::CloseTab { .. } => 27,
        IpcRequest::CloseWindow { .. } => 28,
        IpcRequest::Quit => 26,
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

    fn peer(pid: i32, start_time: u64) -> PeerIdentity {
        PeerIdentity {
            uid: 1000,
            pid,
            start_time,
            token_peer: None,
        }
    }

    #[test]
    fn session_grant_follows_the_peer_not_the_claimed_label() {
        use crate::action::ConsentTier;
        use std::collections::HashSet;

        let granted = peer(111, 5_000);
        // Same uid, different process. This is the attacker: it can claim any
        // label it likes, including the granted client's name.
        let attacker = peer(222, 9_000);
        let mut grants = HashSet::new();
        grants.insert(granted);

        // The label is not an argument to the decision at all, so there is no
        // value it could take that would change these outcomes.
        assert_eq!(
            consent_outcome(ConsentTier::Act, ConsentMode::Ask, granted, &grants, true),
            ConsentOutcome::Allow(AuditDecision::ApprovedSession)
        );
        assert_eq!(
            consent_outcome(ConsentTier::Act, ConsentMode::Ask, attacker, &grants, true),
            ConsentOutcome::Park,
            "a different process must not inherit the grant"
        );
        assert_eq!(
            consent_outcome(ConsentTier::Act, ConsentMode::Ask, attacker, &grants, false),
            ConsentOutcome::Deny(String::from(
                "consent prompt requires an open Mutiny GUI window"
            )),
            "headless, the ungranted peer fails closed"
        );
    }

    #[test]
    fn recycled_pid_does_not_inherit_a_dead_process_grant() {
        use crate::action::ConsentTier;
        use std::collections::HashSet;

        let dead = peer(111, 5_000);
        let recycled = peer(111, 60_000);
        let mut grants = HashSet::new();
        grants.insert(dead);
        assert_ne!(dead, recycled);
        assert_eq!(
            consent_outcome(ConsentTier::Act, ConsentMode::Ask, recycled, &grants, true),
            ConsentOutcome::Park
        );
    }

    /// Remote discovery spends the user's SSH credentials and runs a command on
    /// another machine, so it must not be free on an observe-only carrier.
    #[test]
    fn listing_locally_is_observe_but_naming_a_host_is_act() {
        use crate::action::ConsentTier;
        use std::collections::HashSet;

        let local = IpcRequest::ListSessions {
            host: None,
            refresh: false,
            unattached_only: false,
        };
        let remote = IpcRequest::ListSessions {
            host: Some(String::from("meteor")),
            refresh: false,
            unattached_only: false,
        };
        let open = IpcRequest::OpenSession {
            kind: crate::mux::MuxKind::Tmux,
            name: String::from("work"),
            host: None,
            mode: crate::mux::SessionMode::Tab,
            window: None,
            pane: None,
        };
        assert_eq!(local.consent_tier(), ConsentTier::Observe);
        assert_eq!(remote.consent_tier(), ConsentTier::Act);
        assert_eq!(open.consent_tier(), ConsentTier::Act);

        let grants = HashSet::new();
        let p = peer(4_242, 7);
        assert!(matches!(
            consent_outcome(
                local.consent_tier(),
                ConsentMode::ObserveOnly,
                p,
                &grants,
                true
            ),
            ConsentOutcome::Allow(_)
        ));
        assert!(
            matches!(
                consent_outcome(
                    remote.consent_tier(),
                    ConsentMode::ObserveOnly,
                    p,
                    &grants,
                    true
                ),
                ConsentOutcome::Deny(_)
            ),
            "an observe-only carrier must not get lateral movement for free"
        );
    }

    /// Per-peer consent attribution: a grant installed for one HTTP peer must
    /// not cover another, and a token peer must never collide with a process.
    #[test]
    fn token_peer_grants_are_per_peer_and_never_cross_to_a_process() {
        use crate::action::ConsentTier;
        use std::collections::HashSet;

        let a = PeerIdentity::for_token("token-a", 1000);
        let b = PeerIdentity::for_token("token-b", 1000);
        assert_ne!(a, b, "two tokens must be two identities");
        assert_eq!(
            a,
            PeerIdentity::for_token("token-a", 1000),
            "stable across calls"
        );
        assert!(a.is_token_peer() && b.is_token_peer());
        assert_eq!(a.pid, 0, "a token peer is not a process");
        assert!(!peer(111, 5_000).is_token_peer());
        assert_ne!(a, peer(111, 5_000));

        let mut grants = HashSet::new();
        grants.insert(a);
        assert_eq!(
            consent_outcome(ConsentTier::Act, ConsentMode::Ask, a, &grants, true),
            ConsentOutcome::Allow(AuditDecision::ApprovedSession),
            "the granted peer acts"
        );
        assert_eq!(
            consent_outcome(ConsentTier::Act, ConsentMode::Ask, b, &grants, true),
            ConsentOutcome::Park,
            "another peer must still be asked"
        );
        // A process peer does not inherit a token peer's grant either.
        assert_eq!(
            consent_outcome(
                ConsentTier::Act,
                ConsentMode::Ask,
                peer(111, 5_000),
                &grants,
                true
            ),
            ConsentOutcome::Park
        );
        // Observe-only still denies a token peer regardless of any grant.
        assert!(matches!(
            consent_outcome(ConsentTier::Act, ConsentMode::ObserveOnly, a, &grants, true),
            ConsentOutcome::Deny(_)
        ));
    }

    #[test]
    fn consent_matrix_unchanged_for_non_ask_carriers() {
        use crate::action::ConsentTier;
        use std::collections::HashSet;
        let grants = HashSet::new();
        let p = peer(111, 5_000);

        for carrier in [
            ConsentMode::Operator,
            ConsentMode::Paired,
            ConsentMode::Ask,
            ConsentMode::ObserveOnly,
        ] {
            assert_eq!(
                consent_outcome(ConsentTier::Observe, carrier, p, &grants, false),
                ConsentOutcome::Allow(AuditDecision::Allowed),
                "observe is free on {carrier:?}"
            );
        }
        assert_eq!(
            consent_outcome(ConsentTier::Act, ConsentMode::Operator, p, &grants, false),
            ConsentOutcome::Allow(AuditDecision::Allowed)
        );
        assert_eq!(
            consent_outcome(ConsentTier::Act, ConsentMode::Paired, p, &grants, false),
            ConsentOutcome::Allow(AuditDecision::Allowed)
        );
        assert!(matches!(
            consent_outcome(ConsentTier::Act, ConsentMode::ObserveOnly, p, &grants, true),
            ConsentOutcome::Deny(_)
        ));
    }

    #[test]
    fn irreversible_tier_never_rides_on_session_grants_or_paired_carriers() {
        use crate::action::ConsentTier;
        use std::collections::HashSet;

        let end = IpcRequest::EndSession {
            kind: crate::mux::MuxKind::Tmux,
            name: String::from("prod"),
            host: None,
        };
        let quit = IpcRequest::Quit;
        assert_eq!(end.consent_tier(), ConsentTier::Irreversible);
        assert_eq!(quit.consent_tier(), ConsentTier::Irreversible);

        let granted = peer(111, 5_000);
        let mut grants = HashSet::new();
        grants.insert(granted);

        // Only the operator carrier is the consent itself.
        assert_eq!(
            consent_outcome(
                ConsentTier::Irreversible,
                ConsentMode::Operator,
                granted,
                &grants,
                false
            ),
            ConsentOutcome::Allow(AuditDecision::Allowed)
        );
        // Observe-only refuses outright.
        assert!(matches!(
            consent_outcome(
                ConsentTier::Irreversible,
                ConsentMode::ObserveOnly,
                granted,
                &grants,
                true
            ),
            ConsentOutcome::Deny(_)
        ));
        // Neither a session grant on `ask` nor a `paired` carrier bypasses the
        // per-action prompt: GUI present parks, headless fails closed.
        for carrier in [ConsentMode::Ask, ConsentMode::Paired] {
            assert_eq!(
                consent_outcome(ConsentTier::Irreversible, carrier, granted, &grants, true),
                ConsentOutcome::Park,
                "{carrier:?} must still prompt for Tier-4 Irreversible"
            );
            assert!(
                matches!(
                    consent_outcome(ConsentTier::Irreversible, carrier, granted, &grants, false),
                    ConsentOutcome::Deny(_)
                ),
                "{carrier:?} must fail closed without a GUI"
            );
        }
    }

    #[test]
    fn policy_management_verbs_format_redacted_summaries() {
        let propose = IpcRequest::ProposePolicy {
            description: String::from("safe\x1b[31m grant"),
            verbs: vec![String::from("spawn-tab"), String::from("split")],
            arg_constraints: std::collections::BTreeMap::from([(
                String::from("axis"),
                String::from("horizontal"),
            )]),
            max_calls_per_minute: 30,
            expires_in_minutes: 60,
        };
        let summary = propose.redacted_summary();
        assert!(summary.contains("spawn-tab") && summary.contains("pins=1"));
        assert!(!summary.contains('\x1b'), "control chars must be sanitized");
        assert_eq!(
            IpcRequest::InstallPolicy { id: 42 }.redacted_summary(),
            "id=42"
        );
        assert_eq!(
            IpcRequest::RevokePolicy { id: 7 }.redacted_summary(),
            "id=7"
        );
    }

    #[test]
    fn live_peer_identity_matches_this_process() {
        let sock_path = env::temp_dir().join(format!("mutiny-ident-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = bind_socket(&sock_path).expect("bind");
        let client = UnixStream::connect(&sock_path).expect("connect");
        let (server_side, _) = listener.accept().expect("accept");
        let cred = peer_credentials(&server_side).expect("SO_PEERCRED");
        let identity = PeerIdentity::from_credentials(&cred);

        assert_eq!(identity.uid, server_uid());
        assert_eq!(identity.pid, std::process::id() as i32);
        assert!(
            identity.start_time > 0,
            "starttime should be readable from /proc on Linux"
        );
        assert_eq!(identity.start_time, process_start_time(identity.pid));
        drop(client);
        let _ = std::fs::remove_file(&sock_path);
    }
}
