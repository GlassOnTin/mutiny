//! Session restore: workspace geometry, working directories, and scrollback to
//! a state file, restored on launch.
//!
//! Processes are NOT restored. There is no multiplexer daemon, so the
//! crash-semantics contract stands: a crash kills every mutiny-owned shell.
//! What a restore gives back is the layout, each pane's working directory, and
//! its scrollback as text, with a fresh shell spawned in that directory.
//! Restored scrollback is plain text, not styled cells, because alacritty's
//! grid is not a stable serialization format.
//!
//! A pane that was attached to an external multiplexer is the exception, and
//! not a violation: it RE-ATTACHES to that session. The multiplexer, not
//! mutiny, owns those processes, so their survival was never mutiny's promise.
//! Window order, tab order, and split trees with their ratios are carried
//! verbatim in `WindowModel`, so the arrangement comes back exactly.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::PaneEngine;
use crate::mux::{HostState, MuxInventory, MuxKind, MuxSession};
use crate::peers::{HavenLimbInventory, OutboundLimbStatus, PeersListing};
use crate::topology::{PaneId, WindowId, WindowModel, Workspace};

pub const SESSION_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPane {
    pub id: PaneId,
    pub title: String,
    pub cwd: Option<PathBuf>,
    pub cols: usize,
    pub rows: usize,
    pub scrollback: Vec<String>,
    /// The external multiplexer session this pane was attached to, if any.
    /// Restoring re-attaches it rather than starting a bare shell. Additive and
    /// defaulted, so bundles written before this field still load.
    #[serde(default)]
    pub mux: Option<crate::mux::MuxSessionRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub version: u32,
    /// Reuses the live model types, so split ratios and the tree shape are
    /// carried verbatim rather than re-derived.
    pub windows: Vec<WindowModel>,
    pub active_window: Option<WindowId>,
    pub panes: Vec<SessionPane>,
    pub next_pane_id: u64,
    pub next_tab_id: u64,
    pub next_window_id: u64,
}

pub fn session_path() -> PathBuf {
    session_path_from(
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

pub fn session_path_from(xdg_state_home: Option<&str>, home: Option<&str>) -> PathBuf {
    let base = xdg_state_home
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".local").join("state"))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("mutiny").join("session.json")
}

/// Snapshots geometry, pane metadata, and scrollback. Split ratios ride along
/// inside `WindowModel`'s `SplitNode`.
pub fn capture(ws: &Workspace) -> SessionState {
    let mut panes = Vec::new();
    for win in ws.windows() {
        for tab in &win.tabs {
            for pid in tab.root.leaves() {
                if let Some(eng) = ws.pane(pid) {
                    panes.push(SessionPane {
                        id: pid,
                        title: eng.title().to_owned(),
                        cwd: eng.cwd().map(Path::to_path_buf),
                        cols: eng.size().cols,
                        rows: eng.size().rows,
                        scrollback: eng.scrollback_text(),
                        mux: eng.mux_session().cloned(),
                    });
                }
            }
        }
    }
    let (next_pane_id, next_tab_id, next_window_id) = ws.id_counters();
    SessionState {
        version: SESSION_VERSION,
        windows: ws.windows().to_vec(),
        active_window: ws.active_window_id(),
        panes,
        next_pane_id,
        next_tab_id,
        next_window_id,
    }
}

/// Writes atomically so a crash mid-save cannot leave a corrupt state file.
pub fn save(path: &Path, state: &SessionState) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(state)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// Loads a state file, rejecting an unknown version rather than guessing.
pub fn load(path: &Path) -> Result<Option<SessionState>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    let state: SessionState =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if state.version != SESSION_VERSION {
        return Err(format!(
            "{}: session version {} is not supported (expected {SESSION_VERSION})",
            path.display(),
            state.version
        ));
    }
    Ok(Some(state))
}

/// Restores panes into a workspace using engines the caller spawned. Kept here
/// so the ordering (engines first, then the tree that references them) is in
/// one place.
pub fn install(ws: &mut Workspace, state: SessionState, engines: Vec<(PaneId, PaneEngine)>) {
    let panes = engines.into_iter().collect();
    ws.restore_from_session(
        state.windows,
        state.active_window,
        panes,
        state.next_pane_id,
        state.next_tab_id,
        state.next_window_id,
    );
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRollup {
    pub total_count: usize,
    pub active_count: usize,
    pub busy_count: usize,
    pub attention_count: usize,
}

impl SessionRollup {
    pub fn summary_badge(&self) -> String {
        let mut parts = vec![format!("{}/{} active", self.active_count, self.total_count)];
        if self.busy_count > 0 {
            parts.push(format!("{} busy", self.busy_count));
        }
        if self.attention_count > 0 {
            parts.push(format!("!attention:{}", self.attention_count));
        }
        parts.join(" · ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionGroupKind {
    LocalSessions,
    SshHosts,
    HavenLimb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionNodeKind {
    LocalMux,
    RemoteMux,
    HostStatus,
    HavenSession,
    HavenBridge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTreeItem {
    pub id: String,
    pub node_kind: SessionNodeKind,
    pub name: String,
    pub subtitle: String,
    pub detail: String,
    pub depth: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mux_kind: Option<MuxKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub haven_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen_session_name: Option<String>,
    pub is_agent_repl: bool,
    pub attached: bool,
    pub busy: bool,
    pub attention: bool,
    pub exited: bool,
    pub activatable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTreeGroup {
    pub id: String,
    pub kind: SessionGroupKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    pub rollup: SessionRollup,
    pub items: Vec<SessionTreeItem>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTree {
    pub groups: Vec<SessionTreeGroup>,
    pub scanned_at_ms: u64,
}

type MuxActivityKey = (Option<String>, MuxKind, String);
type MuxActivityMap = HashMap<MuxActivityKey, (bool, bool, bool)>;

fn workspace_mux_activity(ws: &Workspace) -> MuxActivityMap {
    let mut map: MuxActivityMap = HashMap::new();
    for pid in ws.pane_ids() {
        let Some(eng) = ws.pane(pid) else { continue };
        let resolved = eng
            .mux_session()
            .cloned()
            .or_else(|| {
                eng.pty_raw_fd()
                    .and_then(crate::mux::detect_foreground_mux_on_pty)
                    .map(|(kind, name)| crate::mux::MuxSessionRef {
                        kind,
                        name,
                        host: None,
                    })
            })
            .or_else(|| {
                crate::mux::detect_mux_from_text(eng.title()).map(|(kind, name)| {
                    crate::mux::MuxSessionRef {
                        kind,
                        name,
                        host: None,
                    }
                })
            });
        if let Some(mref) = resolved {
            let entry = map
                .entry((mref.host, mref.kind, mref.name))
                .or_insert((false, false, false));
            entry.0 = true;
            if eng.is_command_running() {
                entry.1 = true;
            }
            if eng.attention().is_some() {
                entry.2 = true;
            }
        }
    }
    map
}

/// Builds the unified hierarchical session tree across Local Sessions (with
/// constellation root / prefix nesting), Configured SSH Hosts, and Paired Haven
/// Limbs (`list_sessions` + `list_bridges`), computing group rollups for each.
pub fn build_session_tree(
    ws: &Workspace,
    inventory: &MuxInventory,
    configured_hosts: &[String],
    root_session: Option<&str>,
    peers: &PeersListing,
    limb_inventories: &HashMap<String, HavenLimbInventory>,
) -> SessionTree {
    let pane_activity = workspace_mux_activity(ws);
    let mut groups = Vec::new();

    // 1. Local Sessions (`tmux` / `zellij`, `host == None`).
    let mut local_sessions: Vec<MuxSession> = inventory
        .sessions
        .iter()
        .filter(|s| s.host.is_none())
        .cloned()
        .collect();
    for (host_opt, kind, name) in pane_activity.keys() {
        if host_opt.is_none()
            && !local_sessions
                .iter()
                .any(|s| s.kind == *kind && s.name == *name)
        {
            local_sessions.push(MuxSession {
                host: None,
                kind: *kind,
                name: name.clone(),
                windows: 1,
                attached: 1,
                created: 0,
                path: None,
                exited: false,
            });
        }
    }
    local_sessions.sort_by_key(|s| (s.exited, s.name.clone()));

    // Determine constellation root (either configured `root_session` e.g. `meta`
    // when present alongside sibling live sessions, or `<stem>` prefix heuristic).
    let live_local_names: Vec<String> = local_sessions
        .iter()
        .filter(|s| !s.exited)
        .map(|s| s.name.clone())
        .collect();
    let constellation_root: Option<(String, bool)> = if let Some(cfg_root) = root_session
        && live_local_names.len() >= 2
        && live_local_names.iter().any(|n| n == cfg_root)
    {
        // `true` = nest all live sibling organ sessions under the configured conductor root.
        Some((cfg_root.to_owned(), true))
    } else {
        live_local_names.iter().find_map(|cand| {
            let has_prefixed_children = live_local_names.iter().any(|other| {
                other != cand
                    && other
                        .strip_prefix(cand.as_str())
                        .is_some_and(|rest| rest.starts_with('-') || rest.starts_with('_'))
            });
            has_prefixed_children.then(|| (cand.clone(), false))
        })
    };

    if let Some((ref root_name, nest_all)) = constellation_root {
        local_sessions.sort_by_key(|s| {
            let rank = if !s.exited && &s.name == root_name {
                0u8
            } else if !s.exited
                && (nest_all
                    || s.name
                        .strip_prefix(root_name.as_str())
                        .is_some_and(|r| r.starts_with('-') || r.starts_with('_')))
            {
                1u8
            } else if !s.exited {
                2u8
            } else {
                3u8
            };
            (rank, s.name.clone())
        });
    }

    let mut local_items = Vec::new();
    let mut local_rollup = SessionRollup::default();
    for s in &local_sessions {
        let (ws_attached, busy, attention) = pane_activity
            .get(&(None, s.kind, s.name.clone()))
            .copied()
            .unwrap_or((false, false, false));
        let attached = !s.exited && (s.attached > 0 || ws_attached);
        if !s.exited {
            local_rollup.total_count += 1;
            if attached {
                local_rollup.active_count += 1;
            }
            if busy {
                local_rollup.busy_count += 1;
            }
            if attention {
                local_rollup.attention_count += 1;
            }
        }
        let (depth, parent_session, is_root) = match &constellation_root {
            Some((root_name, _)) if !s.exited && &s.name == root_name => (0u8, None, true),
            Some((root_name, true)) if !s.exited => (1u8, Some(root_name.clone()), false),
            Some((root_name, false))
                if !s.exited
                    && s.name
                        .strip_prefix(root_name.as_str())
                        .is_some_and(|r| r.starts_with('-') || r.starts_with('_')) =>
            {
                (1u8, Some(root_name.clone()), false)
            }
            _ => (0u8, None, false),
        };
        let mut detail_parts = vec![format!(
            "{} · {} window{}",
            s.kind.label(),
            s.windows,
            if s.windows == 1 { "" } else { "s" }
        )];
        if is_root {
            detail_parts.push(String::from("[root]"));
        }
        if attached {
            let count = s.attached.max(u32::from(ws_attached));
            detail_parts.push(format!("attached ×{count}"));
        } else if s.kind == MuxKind::Tmux && !s.exited {
            detail_parts.push(String::from("no clients"));
        }
        if busy {
            detail_parts.push(String::from("busy"));
        }
        if attention {
            detail_parts.push(String::from("!attention"));
        }
        if s.exited {
            detail_parts.push(String::from("exited"));
        }
        local_items.push(SessionTreeItem {
            id: format!("mux:{}:local:{}", s.kind.label(), s.name),
            node_kind: SessionNodeKind::LocalMux,
            name: s.name.clone(),
            subtitle: crate::mux::local_hostname(),
            detail: detail_parts.join(" · "),
            depth,
            parent_session,
            mux_kind: Some(s.kind),
            host: None,
            peer_id: None,
            haven_session_id: None,
            transport: None,
            chosen_session_name: Some(s.name.clone()),
            is_agent_repl: false,
            attached,
            busy,
            attention,
            exited: s.exited,
            activatable: true,
        });
    }
    for h in inventory
        .hosts
        .iter()
        .filter(|h| h.host.is_none() && h.state != HostState::Ok)
    {
        let detail = match h.state {
            HostState::Scanning => format!("{} · scanning…", h.kind.label()),
            _ => format!(
                "{} · {}",
                h.kind.label(),
                h.error.clone().unwrap_or_else(|| String::from("failed"))
            ),
        };
        local_items.push(SessionTreeItem {
            id: format!("host-status:local:{}", h.kind.label()),
            node_kind: SessionNodeKind::HostStatus,
            name: String::from("local"),
            subtitle: detail,
            detail: String::new(),
            depth: 0,
            parent_session: None,
            mux_kind: Some(h.kind),
            host: None,
            peer_id: None,
            haven_session_id: None,
            transport: None,
            chosen_session_name: None,
            is_agent_repl: false,
            attached: false,
            busy: false,
            attention: false,
            exited: false,
            activatable: false,
        });
    }
    if !local_items.is_empty() {
        groups.push(SessionTreeGroup {
            id: String::from("local-sessions"),
            kind: SessionGroupKind::LocalSessions,
            title: String::from("Local Sessions"),
            peer_id: None,
            rollup: local_rollup,
            items: local_items,
        });
    }

    // 2. Configured SSH Hosts (`[sessions] hosts` + remote discovered sessions).
    let mut remote_sessions: Vec<MuxSession> = inventory
        .sessions
        .iter()
        .filter(|s| s.host.is_some())
        .cloned()
        .collect();
    remote_sessions.sort_by_key(|s| (s.exited, s.host.clone(), s.name.clone()));

    let mut ssh_items = Vec::new();
    let mut ssh_rollup = SessionRollup::default();
    let mut covered_hosts = HashSet::new();

    for s in &remote_sessions {
        let host_str = s.host.clone().unwrap_or_default();
        covered_hosts.insert(host_str.clone());
        let (ws_attached, busy, attention) = pane_activity
            .get(&(Some(host_str.clone()), s.kind, s.name.clone()))
            .copied()
            .unwrap_or((false, false, false));
        let attached = !s.exited && (s.attached > 0 || ws_attached);
        if !s.exited {
            ssh_rollup.total_count += 1;
            if attached {
                ssh_rollup.active_count += 1;
            }
            if busy {
                ssh_rollup.busy_count += 1;
            }
            if attention {
                ssh_rollup.attention_count += 1;
            }
        }
        let mut detail_parts = vec![format!(
            "{} · {} window{}",
            s.kind.label(),
            s.windows,
            if s.windows == 1 { "" } else { "s" }
        )];
        if attached {
            let count = s.attached.max(u32::from(ws_attached));
            detail_parts.push(format!("attached ×{count}"));
        } else if s.kind == MuxKind::Tmux && !s.exited {
            detail_parts.push(String::from("no clients"));
        }
        if busy {
            detail_parts.push(String::from("busy"));
        }
        if attention {
            detail_parts.push(String::from("!attention"));
        }
        if s.exited {
            detail_parts.push(String::from("exited"));
        }
        ssh_items.push(SessionTreeItem {
            id: format!("mux:{}:{}:{}", s.kind.label(), host_str, s.name),
            node_kind: SessionNodeKind::RemoteMux,
            name: s.name.clone(),
            subtitle: host_str.clone(),
            detail: detail_parts.join(" · "),
            depth: 0,
            parent_session: None,
            mux_kind: Some(s.kind),
            host: Some(host_str),
            peer_id: None,
            haven_session_id: None,
            transport: Some(String::from("ssh")),
            chosen_session_name: Some(s.name.clone()),
            is_agent_repl: false,
            attached,
            busy,
            attention,
            exited: s.exited,
            activatable: true,
        });
    }

    for h in inventory
        .hosts
        .iter()
        .filter(|h| h.host.is_some() && h.state != HostState::Ok)
    {
        let host_name = h.host.clone().unwrap_or_default();
        covered_hosts.insert(host_name.clone());
        let detail = match h.state {
            HostState::Scanning => format!("{} · scanning…", h.kind.label()),
            _ => format!(
                "{} · {}",
                h.kind.label(),
                h.error
                    .clone()
                    .unwrap_or_else(|| String::from("unavailable"))
            ),
        };
        ssh_items.push(SessionTreeItem {
            id: format!("host-status:{}:{}", host_name, h.kind.label()),
            node_kind: SessionNodeKind::HostStatus,
            name: host_name.clone(),
            subtitle: detail,
            detail: String::new(),
            depth: 0,
            parent_session: None,
            mux_kind: Some(h.kind),
            host: Some(host_name),
            peer_id: None,
            haven_session_id: None,
            transport: Some(String::from("ssh")),
            chosen_session_name: None,
            is_agent_repl: false,
            attached: false,
            busy: false,
            attention: false,
            exited: false,
            activatable: false,
        });
    }

    for cfg_host in configured_hosts {
        if !covered_hosts.contains(cfg_host) {
            ssh_items.push(SessionTreeItem {
                id: format!("host-configured:{cfg_host}"),
                node_kind: SessionNodeKind::HostStatus,
                name: cfg_host.clone(),
                subtitle: String::from("ssh · no active multiplexer sessions"),
                detail: String::new(),
                depth: 0,
                parent_session: None,
                mux_kind: None,
                host: Some(cfg_host.clone()),
                peer_id: None,
                haven_session_id: None,
                transport: Some(String::from("ssh")),
                chosen_session_name: None,
                is_agent_repl: false,
                attached: false,
                busy: false,
                attention: false,
                exited: false,
                activatable: false,
            });
        }
    }

    if !ssh_items.is_empty() {
        groups.push(SessionTreeGroup {
            id: String::from("ssh-hosts"),
            kind: SessionGroupKind::SshHosts,
            title: String::from("SSH Hosts"),
            peer_id: None,
            rollup: ssh_rollup,
            items: ssh_items,
        });
    }

    // 3. Paired Haven Limbs (`● haven:<label>` / `◌ haven:<label>`).
    for out in &peers.outbound {
        let connected = matches!(out.status, OutboundLimbStatus::Connected { .. });
        let prefix = if connected { "●" } else { "◌" };
        let title = format!("{prefix} haven:{}", out.label);
        let limb_inv = limb_inventories.get(&out.id);
        let mut items = Vec::new();
        let mut rollup = SessionRollup {
            total_count: 0,
            active_count: 0,
            busy_count: 0,
            attention_count: out.unread_alert_count,
        };

        if let Some(inv) = limb_inv {
            for hs in &inv.sessions {
                let is_connected = hs.status.eq_ignore_ascii_case("connected");
                rollup.total_count += 1;
                if is_connected {
                    rollup.active_count += 1;
                }
                if hs.is_agent_repl {
                    rollup.busy_count += 1;
                }
                let transport_lc = hs.transport.to_ascii_lowercase();
                let mut detail_parts = vec![transport_lc.clone(), hs.status.to_ascii_lowercase()];
                if let Some(chosen) = &hs.chosen_session_name {
                    let mgr = hs
                        .session_manager
                        .as_deref()
                        .unwrap_or("mux")
                        .to_ascii_lowercase();
                    detail_parts.push(format!("{mgr}:{chosen}"));
                }
                if hs.is_agent_repl {
                    detail_parts.push(String::from("[agent]"));
                }
                let base_name = match &hs.chosen_session_name {
                    Some(chosen) if !chosen.eq_ignore_ascii_case(&hs.label) => {
                        format!("{} ({chosen})", hs.label)
                    }
                    _ => hs.label.clone(),
                };
                let display_name = if hs.is_agent_repl {
                    format!("{base_name} [agent]")
                } else {
                    base_name
                };
                let subtitle = match &hs.chosen_session_name {
                    Some(chosen) => format!("haven:{} · {transport_lc} ({chosen})", out.label),
                    None => format!("haven:{} · {transport_lc}", out.label),
                };
                items.push(SessionTreeItem {
                    id: format!("haven:{}:{}", out.id, hs.session_id),
                    node_kind: SessionNodeKind::HavenSession,
                    name: display_name,
                    subtitle,
                    detail: detail_parts.join(" · "),
                    depth: 0,
                    parent_session: None,
                    mux_kind: hs.session_manager.as_deref().and_then(|m| {
                        if m.eq_ignore_ascii_case("tmux") {
                            Some(MuxKind::Tmux)
                        } else if m.eq_ignore_ascii_case("zellij") {
                            Some(MuxKind::Zellij)
                        } else {
                            None
                        }
                    }),
                    host: None,
                    peer_id: Some(out.id.clone()),
                    haven_session_id: Some(hs.session_id.clone()),
                    transport: Some(transport_lc),
                    chosen_session_name: hs.chosen_session_name.clone(),
                    is_agent_repl: hs.is_agent_repl,
                    attached: is_connected,
                    busy: hs.is_agent_repl,
                    attention: false,
                    exited: false,
                    activatable: connected,
                });
            }

            for (idx, br) in inv.bridges.iter().enumerate() {
                let active = matches!(
                    br.state.to_ascii_lowercase().as_str(),
                    "active" | "connected" | "running" | "streaming" | "exported"
                );
                rollup.total_count += 1;
                if active {
                    rollup.active_count += 1;
                }
                items.push(SessionTreeItem {
                    id: format!("bridge:{}:{idx}:{}", out.id, br.source),
                    node_kind: SessionNodeKind::HavenBridge,
                    name: format!("bridge: {} -> {}", br.source, br.sink),
                    subtitle: format!("haven:{} · {}", out.label, br.source_kind),
                    detail: format!("{} · {}", br.transport, br.state.to_ascii_lowercase()),
                    depth: 0,
                    parent_session: None,
                    mux_kind: None,
                    host: None,
                    peer_id: Some(out.id.clone()),
                    haven_session_id: None,
                    transport: Some(br.transport.clone()),
                    chosen_session_name: None,
                    is_agent_repl: false,
                    attached: active,
                    busy: false,
                    attention: false,
                    exited: false,
                    activatable: false,
                });
            }
        }

        if items.is_empty() {
            let note = if connected {
                "no active phone sessions or bridges"
            } else {
                "limb offline"
            };
            items.push(SessionTreeItem {
                id: format!("haven-empty:{}", out.id),
                node_kind: SessionNodeKind::HostStatus,
                name: format!("haven:{}", out.label),
                subtitle: String::from(note),
                detail: String::new(),
                depth: 0,
                parent_session: None,
                mux_kind: None,
                host: None,
                peer_id: Some(out.id.clone()),
                haven_session_id: None,
                transport: None,
                chosen_session_name: None,
                is_agent_repl: false,
                attached: false,
                busy: false,
                attention: false,
                exited: false,
                activatable: false,
            });
        }

        groups.push(SessionTreeGroup {
            id: format!("haven-limb:{}", out.id),
            kind: SessionGroupKind::HavenLimb,
            title,
            peer_id: Some(out.id.clone()),
            rollup,
            items,
        });
    }

    SessionTree {
        groups,
        scanned_at_ms: inventory.scanned_at_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::SplitAxis;

    fn workspace_with_history() -> (Workspace, PaneId, PaneId) {
        let mut ws = Workspace::new();
        let p1 = ws.reserve_pane_id();
        let mut e1 = PaneEngine::new_headless(40, 6);
        e1.advance_bytes(b"first pane history\r\nline two\r\n");
        let (w1, _t1) = ws.insert_window_with_pane(p1, e1, true);
        let p2 = ws.reserve_pane_id();
        let mut e2 = PaneEngine::new_headless(40, 6);
        e2.advance_bytes(b"second pane history\r\n");
        ws.split_pane_with(p1, SplitAxis::Horizontal, p2, e2)
            .unwrap();
        ws.set_split_ratio(_t1, &[], 0.35);
        let _ = w1;
        (ws, p1, p2)
    }

    #[test]
    fn capture_carries_geometry_ratios_and_scrollback() {
        let (ws, p1, p2) = workspace_with_history();
        let state = capture(&ws);
        assert_eq!(state.version, SESSION_VERSION);
        assert_eq!(state.windows.len(), 1);
        assert_eq!(state.panes.len(), 2);
        let s1 = state.panes.iter().find(|p| p.id == p1).unwrap();
        assert!(
            s1.scrollback
                .iter()
                .any(|l| l.contains("first pane history"))
        );
        let s2 = state.panes.iter().find(|p| p.id == p2).unwrap();
        assert!(
            s2.scrollback
                .iter()
                .any(|l| l.contains("second pane history"))
        );
        match &state.windows[0].tabs[0].root {
            crate::topology::SplitNode::Split { ratio, .. } => {
                assert!((ratio - 0.35).abs() < 1e-9, "ratio must be captured");
            }
            _ => panic!("expected a split"),
        }
    }

    #[test]
    fn save_load_round_trips_and_rejects_bad_version() {
        let dir = std::env::temp_dir().join(format!("mutiny-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");

        assert!(
            load(&path).unwrap().is_none(),
            "missing file is not an error"
        );

        let (ws, _p1, _p2) = workspace_with_history();
        let state = capture(&ws);
        save(&path, &state).unwrap();
        let loaded = load(&path).unwrap().expect("state");
        assert_eq!(loaded.panes.len(), 2);
        assert_eq!(loaded.windows.len(), 1);

        let mut bad = state.clone();
        bad.version = SESSION_VERSION + 1;
        save(&path, &bad).unwrap();
        assert!(load(&path).is_err(), "unknown version must be rejected");

        std::fs::write(&path, "{ not json").unwrap();
        assert!(load(&path).is_err(), "corrupt file must be reported");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_restores_ids_tree_and_counters() {
        let (ws, p1, p2) = workspace_with_history();
        let state = capture(&ws);
        let engines = vec![
            (p1, PaneEngine::new_headless(40, 6)),
            (p2, PaneEngine::new_headless(40, 6)),
        ];
        let mut restored = Workspace::new();
        install(&mut restored, state, engines);
        assert_eq!(restored.windows().len(), 1);
        assert!(restored.pane(p1).is_some());
        assert!(restored.pane(p2).is_some());
        assert_eq!(restored.locate_pane(p2), restored.locate_pane(p1));
        // A new pane must not collide with a restored id.
        let fresh = restored.reserve_pane_id();
        assert!(fresh != p1 && fresh != p2);
    }

    /// A bundle written before the mux field existed must still load, and its
    /// panes must come back as plain shells rather than failing to parse.
    #[test]
    fn a_bundle_without_the_mux_field_still_loads() {
        let (ws, _p1, _p2) = workspace_with_history();
        let state = capture(&ws);
        let mut json = serde_json::to_value(&state).unwrap();
        for pane in json["panes"].as_array_mut().unwrap() {
            pane.as_object_mut().unwrap().remove("mux");
        }
        let text = serde_json::to_string(&json).unwrap();
        assert!(!text.contains("\"mux\""), "fixture must omit the field");
        let loaded: SessionState = serde_json::from_str(&text).expect("legacy bundle loads");
        assert_eq!(loaded.version, SESSION_VERSION, "version stays 1");
        assert!(loaded.panes.iter().all(|p| p.mux.is_none()));
        assert_eq!(loaded.panes.len(), 2);
    }

    #[test]
    fn path_resolution_prefers_xdg_then_home() {
        assert_eq!(
            session_path_from(Some("/state"), Some("/home/u")),
            PathBuf::from("/state/mutiny/session.json")
        );
        assert_eq!(
            session_path_from(None, Some("/home/u")),
            PathBuf::from("/home/u/.local/state/mutiny/session.json")
        );
    }

    #[test]
    fn build_session_tree_nests_constellation_organs_and_rolls_up_activity() {
        use crate::mux::{MuxInventory, MuxKind, MuxSession, MuxSessionRef};
        use crate::peers::{
            HavenBridgeSummary, HavenLimbInventory, HavenRemoteSession, InboundNotifyScope,
            OutboundLimbStatus, OutboundPeerSummary, PeersListing,
        };

        let mut ws = Workspace::new();
        let p_meta = ws.reserve_pane_id();
        let mut eng_meta = PaneEngine::new_headless(40, 10);
        eng_meta.set_mux_session(Some(MuxSessionRef {
            kind: MuxKind::Tmux,
            name: String::from("meta"),
            host: None,
        }));
        ws.insert_window_with_pane(p_meta, eng_meta, true);

        let p_organ = ws.reserve_pane_id();
        let mut eng_organ = PaneEngine::new_headless(40, 10);
        eng_organ.set_mux_session(Some(MuxSessionRef {
            kind: MuxKind::Tmux,
            name: String::from("mutiny"),
            host: None,
        }));
        // Simulate OSC 133;C (busy foreground command) + OSC 777 notification (!attention).
        eng_organ.advance_bytes(b"\x1b]133;C\x07\x1b]777;notify;Build;Green\x07");
        ws.split_pane_with(p_meta, SplitAxis::Horizontal, p_organ, eng_organ)
            .unwrap();

        let inv = MuxInventory {
            sessions: vec![
                MuxSession {
                    host: None,
                    kind: MuxKind::Tmux,
                    name: String::from("meta"),
                    windows: 2,
                    attached: 1,
                    created: 1,
                    path: None,
                    exited: false,
                },
                MuxSession {
                    host: None,
                    kind: MuxKind::Tmux,
                    name: String::from("mutiny"),
                    windows: 1,
                    attached: 1,
                    created: 2,
                    path: None,
                    exited: false,
                },
                MuxSession {
                    host: None,
                    kind: MuxKind::Tmux,
                    name: String::from("radio"),
                    windows: 1,
                    attached: 0,
                    created: 3,
                    path: None,
                    exited: false,
                },
                MuxSession {
                    host: Some(String::from("meteor")),
                    kind: MuxKind::Tmux,
                    name: String::from("trainer"),
                    windows: 3,
                    attached: 0,
                    created: 4,
                    path: None,
                    exited: false,
                },
            ],
            hosts: Vec::new(),
            scanned_at_ms: 1234,
            partial: false,
        };

        let peers = PeersListing {
            inbound: Vec::new(),
            outbound: vec![OutboundPeerSummary {
                id: String::from("hvn-1"),
                label: String::from("pixel"),
                url: String::from("http://127.0.0.1:8740/mcp"),
                inbound_notify_scope: InboundNotifyScope::HavenOnly,
                paired_at: 10,
                last_connected_at: Some(20),
                has_signing_key: false,
                signing_key_id: None,
                rate_limited_count: 0,
                unread_alert_count: 1,
                shade_status: None,
                status: OutboundLimbStatus::Connected {
                    id: String::from("hvn-1"),
                    label: String::from("pixel"),
                    remote_version: Some(String::from("2.5.0")),
                    rtt_ms: 4,
                },
            }],
        };

        let mut limb_invs = HashMap::new();
        limb_invs.insert(
            String::from("hvn-1"),
            HavenLimbInventory {
                sessions: vec![HavenRemoteSession {
                    session_id: String::from("sess-9"),
                    profile_id: String::from("prof-1"),
                    label: String::from("Claude Pocket"),
                    status: String::from("CONNECTED"),
                    transport: String::from("SSH"),
                    is_agent_repl: true,
                    session_manager: Some(String::from("tmux")),
                    chosen_session_name: Some(String::from("mutiny")),
                }],
                bridges: vec![HavenBridgeSummary {
                    source: String::from("usb:1050:0407"),
                    source_kind: String::from("usb"),
                    sink: String::from("tcp:127.0.0.1:3240"),
                    transport: String::from("usbip"),
                    state: String::from("active"),
                }],
            },
        );

        let tree = build_session_tree(
            &ws,
            &inv,
            &[String::from("meteor"), String::from("bastion")],
            Some("meta"),
            &peers,
            &limb_invs,
        );
        assert_eq!(tree.groups.len(), 3);

        let local = &tree.groups[0];
        assert_eq!(local.kind, SessionGroupKind::LocalSessions);
        assert_eq!(local.rollup.total_count, 3);
        assert_eq!(local.rollup.active_count, 2);
        assert_eq!(local.rollup.busy_count, 1);
        assert_eq!(local.rollup.attention_count, 1);
        assert_eq!(local.items[0].name, "meta");
        assert_eq!(local.items[0].depth, 0);
        assert_eq!(local.items[1].name, "mutiny");
        assert_eq!(local.items[1].depth, 1);
        assert_eq!(local.items[1].parent_session.as_deref(), Some("meta"));
        assert!(local.items[1].busy && local.items[1].attention);
        assert_eq!(local.items[2].name, "radio");
        assert_eq!(local.items[2].depth, 1);

        let ssh = &tree.groups[1];
        assert_eq!(ssh.kind, SessionGroupKind::SshHosts);
        assert_eq!(ssh.rollup.total_count, 1);
        assert!(
            ssh.items
                .iter()
                .any(|i| i.name == "trainer" && i.activatable)
        );
        assert!(
            ssh.items
                .iter()
                .any(|i| i.name == "bastion" && !i.activatable)
        );

        let haven = &tree.groups[2];
        assert_eq!(haven.kind, SessionGroupKind::HavenLimb);
        assert_eq!(haven.title, "● haven:pixel");
        assert_eq!(haven.rollup.total_count, 2);
        assert_eq!(haven.rollup.active_count, 2);
        assert_eq!(haven.rollup.attention_count, 1);
        assert!(haven.items[0].is_agent_repl);
        assert!(haven.items[0].detail.contains("[agent]"));
        assert_eq!(
            haven.items[0].chosen_session_name.as_deref(),
            Some("mutiny")
        );
        assert_eq!(haven.items[1].node_kind, SessionNodeKind::HavenBridge);
    }
}
