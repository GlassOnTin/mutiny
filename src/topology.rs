use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::engine::{AttentionReason, PaneEngine, ProgressState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaneId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TabId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowId(pub u64);

impl fmt::Display for PaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SplitAxis {
    Horizontal, // left | right (SplitRight)
    Vertical,   // top / bottom (SplitDown)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SplitNode {
    Leaf(PaneId),
    Split {
        axis: SplitAxis,
        ratio: f64,
        first: Box<SplitNode>,
        second: Box<SplitNode>,
    },
}

impl SplitNode {
    pub fn leaves(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            SplitNode::Leaf(id) => out.push(*id),
            SplitNode::Split { first, second, .. } => {
                first.collect_leaves(out);
                second.collect_leaves(out);
            }
        }
    }

    pub fn contains(&self, target: PaneId) -> bool {
        match self {
            SplitNode::Leaf(id) => *id == target,
            SplitNode::Split { first, second, .. } => {
                first.contains(target) || second.contains(target)
            }
        }
    }

    pub fn first_leaf(&self) -> PaneId {
        match self {
            SplitNode::Leaf(id) => *id,
            SplitNode::Split { first, .. } => first.first_leaf(),
        }
    }

    pub fn split_leaf(&mut self, target: PaneId, axis: SplitAxis, new_pane: PaneId) -> bool {
        match self {
            SplitNode::Leaf(id) if *id == target => {
                let old = *id;
                *self = SplitNode::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(SplitNode::Leaf(old)),
                    second: Box::new(SplitNode::Leaf(new_pane)),
                };
                true
            }
            SplitNode::Leaf(_) => false,
            SplitNode::Split { first, second, .. } => {
                first.split_leaf(target, axis, new_pane)
                    || second.split_leaf(target, axis, new_pane)
            }
        }
    }

    /// Removes `target` from this tree, returning `(remaining_tree, did_remove)`.
    pub fn remove_leaf(self, target: PaneId) -> (Option<SplitNode>, bool) {
        match self {
            SplitNode::Leaf(id) => {
                if id == target {
                    (None, true)
                } else {
                    (Some(SplitNode::Leaf(id)), false)
                }
            }
            SplitNode::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (new_first, removed_in_first) = first.remove_leaf(target);
                if removed_in_first {
                    return match new_first {
                        Some(f) => (
                            Some(SplitNode::Split {
                                axis,
                                ratio,
                                first: Box::new(f),
                                second,
                            }),
                            true,
                        ),
                        None => (Some(*second), true),
                    };
                }
                let kept_first = Box::new(new_first.expect("unchanged subtree must exist"));
                let (new_second, removed_in_second) = second.remove_leaf(target);
                if removed_in_second {
                    return match new_second {
                        Some(s) => (
                            Some(SplitNode::Split {
                                axis,
                                ratio,
                                first: kept_first,
                                second: Box::new(s),
                            }),
                            true,
                        ),
                        None => (Some(*kept_first), true),
                    };
                }
                let kept_second = Box::new(new_second.expect("unchanged subtree must exist"));
                (
                    Some(SplitNode::Split {
                        axis,
                        ratio,
                        first: kept_first,
                        second: kept_second,
                    }),
                    false,
                )
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabModel {
    pub id: TabId,
    pub root: SplitNode,
    pub active_pane: PaneId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowModel {
    pub id: WindowId,
    pub tabs: Vec<TabModel>,
    pub active_tab: TabId,
    pub menubar_visible: bool,
}

impl WindowModel {
    pub fn active_tab_ref(&self) -> Option<&TabModel> {
        self.tabs.iter().find(|t| t.id == self.active_tab)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut TabModel> {
        self.tabs.iter_mut().find(|t| t.id == self.active_tab)
    }

    pub fn active_pane(&self) -> Option<PaneId> {
        self.active_tab_ref().map(|t| t.active_pane)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopologyError {
    PaneNotFound(PaneId),
    TabNotFound(TabId),
    WindowNotFound(WindowId),
    AlreadyStandaloneWindow(PaneId),
    SameTabMove(PaneId, TabId),
}

impl fmt::Display for TopologyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TopologyError::PaneNotFound(p) => write!(f, "pane {p} not found"),
            TopologyError::TabNotFound(t) => write!(f, "tab {} not found", t.0),
            TopologyError::WindowNotFound(w) => write!(f, "window {} not found", w.0),
            TopologyError::AlreadyStandaloneWindow(p) => {
                write!(f, "pane {p} is already the sole pane in its window")
            }
            TopologyError::SameTabMove(p, t) => {
                write!(f, "pane {p} is already the sole pane of tab {}", t.0)
            }
        }
    }
}

impl std::error::Error for TopologyError {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub active_window: Option<WindowId>,
    pub windows: Vec<WindowSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowSummary {
    pub id: WindowId,
    pub active_tab: TabId,
    pub menubar_visible: bool,
    pub tabs: Vec<TabSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabSummary {
    pub id: TabId,
    pub active_pane: PaneId,
    pub tree: SplitNode,
    pub panes: Vec<PaneSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneSummary {
    pub id: PaneId,
    pub title: String,
    pub cwd: Option<String>,
    pub command_running: bool,
    pub attention: Option<AttentionReason>,
    pub agent_notice: Option<String>,
    pub progress: ProgressState,
    pub prompt_seq: u64,
    pub output_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitedPaneRecord {
    pub id: PaneId,
    pub title: String,
    pub cwd: Option<String>,
    pub exit_code: Option<i32>,
    pub cols: usize,
    pub rows: usize,
    pub screen: Vec<String>,
}

const MAX_EXITED_PANES: usize = 8;

pub struct Workspace {
    panes: BTreeMap<PaneId, PaneEngine>,
    windows: Vec<WindowModel>,
    active_window: Option<WindowId>,
    exited_panes: VecDeque<ExitedPaneRecord>,
    next_pane_id: u64,
    next_tab_id: u64,
    next_window_id: u64,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace {
    pub fn new() -> Self {
        Self {
            panes: BTreeMap::new(),
            windows: Vec::new(),
            active_window: None,
            exited_panes: VecDeque::with_capacity(MAX_EXITED_PANES),
            next_pane_id: 1,
            next_tab_id: 1,
            next_window_id: 1,
        }
    }

    pub fn reserve_pane_id(&mut self) -> PaneId {
        let id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        id
    }

    fn alloc_tab_id(&mut self) -> TabId {
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        id
    }

    fn alloc_window_id(&mut self) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id += 1;
        id
    }

    pub fn windows(&self) -> &[WindowModel] {
        &self.windows
    }

    pub fn window(&self, id: WindowId) -> Option<&WindowModel> {
        self.windows.iter().find(|w| w.id == id)
    }

    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut WindowModel> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    pub fn active_window_id(&self) -> Option<WindowId> {
        self.active_window
    }

    pub fn set_active_window(&mut self, id: WindowId) {
        if self.windows.iter().any(|w| w.id == id) {
            self.active_window = Some(id);
        }
    }

    pub fn active_pane_id(&self) -> Option<PaneId> {
        let win_id = self.active_window?;
        self.window(win_id)?.active_pane()
    }

    pub fn pane(&self, id: PaneId) -> Option<&PaneEngine> {
        self.panes.get(&id)
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut PaneEngine> {
        self.panes.get_mut(&id)
    }

    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.panes.keys().copied().collect()
    }

    pub fn locate_pane(&self, target: PaneId) -> Option<(WindowId, TabId)> {
        for win in &self.windows {
            for tab in &win.tabs {
                if tab.root.contains(target) {
                    return Some((win.id, tab.id));
                }
            }
        }
        None
    }

    pub fn insert_window_with_pane(
        &mut self,
        pane_id: PaneId,
        engine: PaneEngine,
        menubar_visible: bool,
    ) -> (WindowId, TabId) {
        self.panes.insert(pane_id, engine);
        let tab_id = self.alloc_tab_id();
        let win_id = self.alloc_window_id();
        let tab = TabModel {
            id: tab_id,
            root: SplitNode::Leaf(pane_id),
            active_pane: pane_id,
        };
        let win = WindowModel {
            id: win_id,
            tabs: vec![tab],
            active_tab: tab_id,
            menubar_visible,
        };
        self.windows.push(win);
        self.active_window = Some(win_id);
        (win_id, tab_id)
    }

    pub fn insert_tab_with_pane(
        &mut self,
        win_id: WindowId,
        pane_id: PaneId,
        engine: PaneEngine,
    ) -> Result<TabId, TopologyError> {
        if !self.windows.iter().any(|w| w.id == win_id) {
            return Err(TopologyError::WindowNotFound(win_id));
        }
        self.panes.insert(pane_id, engine);
        let tab_id = self.alloc_tab_id();
        let win = self.window_mut(win_id).unwrap();
        win.tabs.push(TabModel {
            id: tab_id,
            root: SplitNode::Leaf(pane_id),
            active_pane: pane_id,
        });
        win.active_tab = tab_id;
        self.active_window = Some(win_id);
        Ok(tab_id)
    }

    pub fn split_pane_with(
        &mut self,
        target: PaneId,
        axis: SplitAxis,
        new_pane_id: PaneId,
        engine: PaneEngine,
    ) -> Result<(WindowId, TabId), TopologyError> {
        let Some((win_id, tab_id)) = self.locate_pane(target) else {
            return Err(TopologyError::PaneNotFound(target));
        };
        self.panes.insert(new_pane_id, engine);
        let win = self.window_mut(win_id).unwrap();
        let tab = win.tabs.iter_mut().find(|t| t.id == tab_id).unwrap();
        let ok = tab.root.split_leaf(target, axis, new_pane_id);
        debug_assert!(ok);
        tab.active_pane = new_pane_id;
        win.active_tab = tab_id;
        self.active_window = Some(win_id);
        Ok((win_id, tab_id))
    }

    /// Detaches `pane_id` into a new top-level `WindowModel` without touching its `PaneEngine`/PTY.
    pub fn detach_split(&mut self, pane_id: PaneId) -> Result<(WindowId, TabId), TopologyError> {
        let Some((src_win_id, src_tab_id)) = self.locate_pane(pane_id) else {
            return Err(TopologyError::PaneNotFound(pane_id));
        };

        let menubar_visible = {
            let src_win = self.window(src_win_id).unwrap();
            let src_tab = src_win.tabs.iter().find(|t| t.id == src_tab_id).unwrap();
            if src_win.tabs.len() == 1 && src_tab.root == SplitNode::Leaf(pane_id) {
                return Err(TopologyError::AlreadyStandaloneWindow(pane_id));
            }
            src_win.menubar_visible
        };

        self.unlink_pane_from_tree(src_win_id, src_tab_id, pane_id);

        let new_tab_id = self.alloc_tab_id();
        let new_win_id = self.alloc_window_id();
        self.windows.push(WindowModel {
            id: new_win_id,
            tabs: vec![TabModel {
                id: new_tab_id,
                root: SplitNode::Leaf(pane_id),
                active_pane: pane_id,
            }],
            active_tab: new_tab_id,
            menubar_visible,
        });
        self.active_window = Some(new_win_id);
        Ok((new_win_id, new_tab_id))
    }

    /// Moves `pane_id` from any split/tab/window into `dst_tab_id` split alongside its active pane.
    pub fn move_pane_to_tab(
        &mut self,
        pane_id: PaneId,
        dst_tab_id: TabId,
        axis: SplitAxis,
    ) -> Result<WindowId, TopologyError> {
        let Some((src_win_id, src_tab_id)) = self.locate_pane(pane_id) else {
            return Err(TopologyError::PaneNotFound(pane_id));
        };
        let Some(dst_win_id) = self
            .windows
            .iter()
            .find(|w| w.tabs.iter().any(|t| t.id == dst_tab_id))
            .map(|w| w.id)
        else {
            return Err(TopologyError::TabNotFound(dst_tab_id));
        };

        if src_tab_id == dst_tab_id {
            let tab = self
                .window(src_win_id)
                .unwrap()
                .tabs
                .iter()
                .find(|t| t.id == src_tab_id)
                .unwrap();
            if tab.root == SplitNode::Leaf(pane_id) {
                return Err(TopologyError::SameTabMove(pane_id, dst_tab_id));
            }
        }

        self.unlink_pane_from_tree(src_win_id, src_tab_id, pane_id);

        let dst_win = self.window_mut(dst_win_id).unwrap();
        let dst_tab = dst_win
            .tabs
            .iter_mut()
            .find(|t| t.id == dst_tab_id)
            .unwrap();
        let anchor = dst_tab.active_pane;
        dst_tab.root.split_leaf(anchor, axis, pane_id);
        dst_tab.active_pane = pane_id;
        dst_win.active_tab = dst_tab_id;
        self.active_window = Some(dst_win_id);
        Ok(dst_win_id)
    }

    pub fn focus_pane(&mut self, pane_id: PaneId) -> Result<(WindowId, TabId), TopologyError> {
        let Some((win_id, tab_id)) = self.locate_pane(pane_id) else {
            return Err(TopologyError::PaneNotFound(pane_id));
        };
        let win = self.window_mut(win_id).unwrap();
        let tab = win.tabs.iter_mut().find(|t| t.id == tab_id).unwrap();
        tab.active_pane = pane_id;
        win.active_tab = tab_id;
        self.active_window = Some(win_id);
        if let Some(engine) = self.panes.get_mut(&pane_id) {
            engine.clear_attention();
        }
        Ok((win_id, tab_id))
    }

    pub fn cycle_pane_in_active_tab(&mut self, win_id: WindowId, forward: bool) -> Option<PaneId> {
        let next_pane = {
            let win = self.window(win_id)?;
            let tab = win.active_tab_ref()?;
            let leaves = tab.root.leaves();
            if leaves.is_empty() {
                return None;
            }
            let pos = leaves
                .iter()
                .position(|&p| p == tab.active_pane)
                .unwrap_or(0);
            let next_idx = if forward {
                (pos + 1) % leaves.len()
            } else {
                (pos + leaves.len() - 1) % leaves.len()
            };
            leaves[next_idx]
        };
        let _ = self.focus_pane(next_pane);
        Some(next_pane)
    }

    /// Jumps across windows, tabs, and splits to the next pane carrying an `AttentionReason`.
    pub fn focus_urgent_next(&mut self) -> Option<(WindowId, TabId, PaneId)> {
        let mut ordered = Vec::new();
        for win in &self.windows {
            for tab in &win.tabs {
                for pane_id in tab.root.leaves() {
                    ordered.push((win.id, tab.id, pane_id));
                }
            }
        }
        if ordered.is_empty() {
            return None;
        }

        let current = self.active_pane_id();
        let start_idx = current
            .and_then(|cur| ordered.iter().position(|(_, _, p)| *p == cur))
            .map(|i| (i + 1) % ordered.len())
            .unwrap_or(0);

        for offset in 0..ordered.len() {
            let (win_id, tab_id, pane_id) = ordered[(start_idx + offset) % ordered.len()];
            if self
                .panes
                .get(&pane_id)
                .and_then(|e| e.attention())
                .is_some()
            {
                let _ = self.focus_pane(pane_id);
                return Some((win_id, tab_id, pane_id));
            }
        }
        None
    }

    pub fn close_pane(&mut self, pane_id: PaneId) -> Option<PaneEngine> {
        let (win_id, tab_id) = self.locate_pane(pane_id)?;
        self.unlink_pane_from_tree(win_id, tab_id, pane_id);
        let removed = self.panes.remove(&pane_id)?;
        self.record_exited_pane(pane_id, &removed);
        Some(removed)
    }

    pub fn close_window(&mut self, win_id: WindowId) -> Vec<PaneEngine> {
        let Some(pos) = self.windows.iter().position(|w| w.id == win_id) else {
            return Vec::new();
        };
        let removed_win = self.windows.remove(pos);
        let mut removed_engines = Vec::new();
        for tab in removed_win.tabs {
            for pane_id in tab.root.leaves() {
                if let Some(eng) = self.panes.remove(&pane_id) {
                    self.record_exited_pane(pane_id, &eng);
                    removed_engines.push(eng);
                }
            }
        }
        if self.active_window == Some(win_id) {
            self.active_window = self.windows.last().map(|w| w.id);
        }
        removed_engines
    }

    fn record_exited_pane(&mut self, id: PaneId, eng: &PaneEngine) {
        if self.exited_panes.len() >= MAX_EXITED_PANES {
            self.exited_panes.pop_front();
        }
        self.exited_panes.push_back(ExitedPaneRecord {
            id,
            title: eng.title().to_owned(),
            cwd: eng.cwd().map(|p| p.display().to_string()),
            exit_code: eng
                .last_child_exit_code()
                .or_else(|| eng.last_command().and_then(|c| c.exit_code)),
            cols: eng.size().cols,
            rows: eng.size().rows,
            screen: eng.trimmed_screen_lines(),
        });
    }

    pub fn exited_panes(&self) -> Vec<ExitedPaneRecord> {
        self.exited_panes.iter().rev().cloned().collect()
    }

    pub fn read_exited(&self, pane: Option<PaneId>) -> Option<ExitedPaneRecord> {
        match pane {
            Some(target) => self
                .exited_panes
                .iter()
                .rev()
                .find(|r| r.id == target)
                .cloned(),
            None => self.exited_panes.back().cloned(),
        }
    }

    fn unlink_pane_from_tree(&mut self, win_id: WindowId, tab_id: TabId, pane_id: PaneId) {
        let Some(win_idx) = self.windows.iter().position(|w| w.id == win_id) else {
            return;
        };
        let Some(tab_idx) = self.windows[win_idx]
            .tabs
            .iter()
            .position(|t| t.id == tab_id)
        else {
            return;
        };

        let old_root = self.windows[win_idx].tabs[tab_idx].root.clone();
        let (maybe_new_root, _) = old_root.remove_leaf(pane_id);

        match maybe_new_root {
            Some(new_root) => {
                let tab = &mut self.windows[win_idx].tabs[tab_idx];
                if tab.active_pane == pane_id {
                    tab.active_pane = new_root.first_leaf();
                }
                tab.root = new_root;
            }
            None => {
                self.windows[win_idx].tabs.remove(tab_idx);
                if self.windows[win_idx].tabs.is_empty() {
                    self.windows.remove(win_idx);
                    if self.active_window == Some(win_id) {
                        self.active_window = self.windows.last().map(|w| w.id);
                    }
                } else if self.windows[win_idx].active_tab == tab_id {
                    let next_idx = tab_idx.min(self.windows[win_idx].tabs.len() - 1);
                    self.windows[win_idx].active_tab = self.windows[win_idx].tabs[next_idx].id;
                }
            }
        }
    }

    pub fn summary(&self) -> WorkspaceSummary {
        let windows = self
            .windows
            .iter()
            .map(|w| WindowSummary {
                id: w.id,
                active_tab: w.active_tab,
                menubar_visible: w.menubar_visible,
                tabs: w
                    .tabs
                    .iter()
                    .map(|t| {
                        let panes = t
                            .root
                            .leaves()
                            .into_iter()
                            .filter_map(|pid| {
                                let eng = self.panes.get(&pid)?;
                                Some(PaneSummary {
                                    id: pid,
                                    title: eng.title().to_owned(),
                                    cwd: eng.cwd().map(|p| p.display().to_string()),
                                    command_running: eng.is_command_running(),
                                    attention: eng.attention(),
                                    agent_notice: eng.agent_notice().map(str::to_owned),
                                    progress: eng.progress(),
                                    prompt_seq: eng.prompt_seq(),
                                    output_seq: eng.output_seq(),
                                })
                            })
                            .collect();
                        TabSummary {
                            id: t.id,
                            active_pane: t.active_pane,
                            tree: t.root.clone(),
                            panes,
                        }
                    })
                    .collect(),
            })
            .collect();

        WorkspaceSummary {
            active_window: self.active_window,
            windows,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_split_preserves_pane_engine_and_collapses_source_tree() {
        let mut ws = Workspace::new();
        let p1 = ws.reserve_pane_id();
        let (w1, t1) = ws.insert_window_with_pane(p1, PaneEngine::new_headless(40, 10), true);

        let p2 = ws.reserve_pane_id();
        let mut eng2 = PaneEngine::new_headless(40, 10);
        eng2.advance_bytes(b"survivor-scrollback");
        ws.split_pane_with(p1, SplitAxis::Horizontal, p2, eng2)
            .unwrap();

        let p3 = ws.reserve_pane_id();
        ws.split_pane_with(
            p2,
            SplitAxis::Vertical,
            p3,
            PaneEngine::new_headless(40, 10),
        )
        .unwrap();

        assert_eq!(
            ws.window(w1)
                .unwrap()
                .active_tab_ref()
                .unwrap()
                .root
                .leaves(),
            vec![p1, p2, p3]
        );

        let (w2, _t2) = ws.detach_split(p2).expect("detach_split should succeed");
        assert_ne!(w1, w2);
        assert_eq!(ws.windows().len(), 2);
        assert_eq!(ws.window(w1).unwrap().tabs[0].root.leaves(), vec![p1, p3]);
        assert_eq!(ws.window(w2).unwrap().tabs[0].root.leaves(), vec![p2]);
        assert!(
            ws.pane(p2)
                .unwrap()
                .screen_text()
                .contains("survivor-scrollback")
        );

        // Sole pane in w2 cannot detach further
        assert_eq!(
            ws.detach_split(p2),
            Err(TopologyError::AlreadyStandaloneWindow(p2))
        );

        // Move p2 back into t1!
        ws.move_pane_to_tab(p2, t1, SplitAxis::Horizontal).unwrap();
        assert_eq!(ws.windows().len(), 1);
        assert_eq!(ws.window(w1).unwrap().tabs[0].root.leaves().len(), 3);
    }

    #[test]
    fn urgent_next_routes_across_windows_and_clears_attention() {
        let mut ws = Workspace::new();
        let p1 = ws.reserve_pane_id();
        ws.insert_window_with_pane(p1, PaneEngine::new_headless(30, 5), true);

        let p2 = ws.reserve_pane_id();
        let mut failing = PaneEngine::new_headless(30, 5);
        failing.advance_bytes(b"\x1b]133;C\x07boom\r\n\x1b]133;D;2\x07");
        let (w2, t2) = ws.insert_window_with_pane(p2, failing, true);

        ws.focus_pane(p1).unwrap();
        let routed = ws.focus_urgent_next();
        assert_eq!(routed, Some((w2, t2, p2)));
        assert_eq!(ws.active_pane_id(), Some(p2));
        assert_eq!(ws.pane(p2).unwrap().attention(), None);
        assert_eq!(ws.focus_urgent_next(), None);
    }

    #[test]
    fn retains_last_eight_exited_panes_with_final_screen() {
        let mut ws = Workspace::new();
        for i in 0..10 {
            let pid = ws.reserve_pane_id();
            let mut eng = PaneEngine::new_headless(30, 4);
            eng.advance_bytes(
                format!("fatal-startup-{i}\r\n\x1b]133;C\x07\x1b]133;D;127\x07").as_bytes(),
            );
            ws.insert_window_with_pane(pid, eng, true);
            ws.close_pane(pid);
        }
        assert_eq!(ws.exited_panes().len(), 8);
        let latest = ws.read_exited(None).expect("latest exited pane");
        assert_eq!(latest.id, PaneId(10));
        assert_eq!(latest.exit_code, Some(127));
        assert_eq!(latest.screen, vec!["fatal-startup-9"]);
        assert!(ws.read_exited(Some(PaneId(1))).is_none());
    }
}
