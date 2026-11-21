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

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::PaneEngine;
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
}
