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
    /// Builds a balanced `cols x rows` binary split tree for `1..=N` panes.
    pub fn balanced_grid(panes: &[PaneId]) -> Option<SplitNode> {
        if panes.is_empty() {
            return None;
        }
        let cols = (panes.len() as f64).sqrt().ceil() as usize;
        let cols = cols.max(1);
        let row_nodes: Vec<SplitNode> = panes
            .chunks(cols)
            .filter_map(|row| {
                let leaves: Vec<SplitNode> = row.iter().copied().map(SplitNode::Leaf).collect();
                Self::chain_balanced(leaves, SplitAxis::Horizontal)
            })
            .collect();
        Self::chain_balanced(row_nodes, SplitAxis::Vertical)
    }

    fn chain_balanced(mut items: Vec<SplitNode>, axis: SplitAxis) -> Option<SplitNode> {
        let len = items.len();
        if len == 0 {
            return None;
        }
        let first = items.remove(0);
        if items.is_empty() {
            return Some(first);
        }
        let ratio = (1.0 / (len as f64)).clamp(0.05, 0.95);
        let rest = Self::chain_balanced(items, axis)?;
        Some(SplitNode::Split {
            axis,
            ratio,
            first: Box::new(first),
            second: Box::new(rest),
        })
    }

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

    /// Sets the divider ratio at a path of `first` (false) / `second` (true)
    /// choices from this node. Clamped so a pane can never be dragged to zero.
    /// Returns false if the path does not land on a split.
    pub fn set_ratio_at_path(&mut self, path: &[bool], ratio: f64) -> bool {
        match self {
            SplitNode::Leaf(_) => false,
            SplitNode::Split {
                first,
                second,
                ratio: slot,
                ..
            } => {
                if path.is_empty() {
                    *slot = ratio.clamp(0.05, 0.95);
                    true
                } else if path[0] {
                    second.set_ratio_at_path(&path[1..], ratio)
                } else {
                    first.set_ratio_at_path(&path[1..], ratio)
                }
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
    /// The summonable quick terminal. A WM close dismisses it instead of
    /// ending its shell, so the flag travels with the saved layout.
    #[serde(default)]
    pub quick: bool,
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
    /// Whether the quick terminal is currently summoned. The controller fills
    /// this in, because the summoned flag lives there rather than in the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick_visible: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowSummary {
    pub id: WindowId,
    pub active_tab: TabId,
    pub menubar_visible: bool,
    /// The summonable scratch window, whose WM close dismisses rather than ends it.
    #[serde(default)]
    pub quick: bool,
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

    /// Replaces the workspace with a restored session. Ids and counters come
    /// from the state file so a restored bundle keeps its identity; engines are
    /// supplied by the caller because spawning a PTY is not the model's job.
    pub fn restore_from_session(
        &mut self,
        windows: Vec<WindowModel>,
        active_window: Option<WindowId>,
        panes: BTreeMap<PaneId, PaneEngine>,
        next_pane_id: u64,
        next_tab_id: u64,
        next_window_id: u64,
    ) {
        self.panes = panes;
        self.windows = windows;
        self.active_window = active_window.filter(|id| self.windows.iter().any(|w| w.id == *id));
        self.exited_panes.clear();
        self.next_pane_id = next_pane_id;
        self.next_tab_id = next_tab_id;
        self.next_window_id = next_window_id;
    }

    pub fn id_counters(&self) -> (u64, u64, u64) {
        (self.next_pane_id, self.next_tab_id, self.next_window_id)
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
            quick: false,
        };
        self.windows.push(win);
        self.active_window = Some(win_id);
        (win_id, tab_id)
    }

    pub fn insert_window_with_grid(
        &mut self,
        entries: Vec<(PaneId, PaneEngine)>,
        menubar_visible: bool,
    ) -> Option<(WindowId, TabId, PaneId)> {
        let pane_ids: Vec<PaneId> = entries.iter().map(|(id, _)| *id).collect();
        let root = SplitNode::balanced_grid(&pane_ids)?;
        let first_pane = pane_ids[0];
        for (pid, engine) in entries {
            self.panes.insert(pid, engine);
        }
        let tab_id = self.alloc_tab_id();
        let win_id = self.alloc_window_id();
        self.windows.push(WindowModel {
            id: win_id,
            tabs: vec![TabModel {
                id: tab_id,
                root,
                active_pane: first_pane,
            }],
            active_tab: tab_id,
            menubar_visible,
            quick: false,
        });
        self.active_window = Some(win_id);
        Some((win_id, tab_id, first_pane))
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
            quick: false,
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

    /// Writes a divider ratio back to the tree, so a user drag survives widget
    /// rebuilds, tab switches, and reparenting.
    pub fn set_split_ratio(&mut self, tab_id: TabId, path: &[bool], ratio: f64) -> bool {
        for win in &mut self.windows {
            if let Some(tab) = win.tabs.iter_mut().find(|t| t.id == tab_id) {
                return tab.root.set_ratio_at_path(path, ratio);
            }
        }
        false
    }

    /// Moves a tab to a new index within its window, for drag-to-reorder.
    pub fn reorder_tab(
        &mut self,
        win_id: WindowId,
        tab_id: TabId,
        new_index: usize,
    ) -> Result<(), TopologyError> {
        let win = self
            .window_mut(win_id)
            .ok_or(TopologyError::WindowNotFound(win_id))?;
        let pos = win
            .tabs
            .iter()
            .position(|t| t.id == tab_id)
            .ok_or(TopologyError::TabNotFound(tab_id))?;
        let target = new_index.min(win.tabs.len().saturating_sub(1));
        if pos == target {
            return Ok(());
        }
        let tab = win.tabs.remove(pos);
        win.tabs.insert(target, tab);
        Ok(())
    }

    /// Moves `tab_id` (including its entire split tree and running `PaneEngine`s)
    /// into `dst_win_id` at `dst_index` (`None` appends at the end). If `tab_id`
    /// already lives in `dst_win_id`, reorders it within that window. If moving
    /// the last tab out of a non-quick source window leaves that source window
    /// empty, removes the empty source window and returns its `WindowId` so the
    /// controller can close or reconcile it.
    pub fn move_tab_to_window(
        &mut self,
        tab_id: TabId,
        dst_win_id: WindowId,
        dst_index: Option<usize>,
    ) -> Result<WindowId, TopologyError> {
        let Some((src_win_idx, src_tab_pos)) =
            self.windows.iter().enumerate().find_map(|(wi, w)| {
                w.tabs
                    .iter()
                    .position(|t| t.id == tab_id)
                    .map(|ti| (wi, ti))
            })
        else {
            return Err(TopologyError::TabNotFound(tab_id));
        };
        let Some(dst_is_quick) = self.window(dst_win_id).map(|w| w.quick) else {
            return Err(TopologyError::WindowNotFound(dst_win_id));
        };

        let src_win_id = self.windows[src_win_idx].id;
        if src_win_id == dst_win_id {
            let len = self.windows[src_win_idx].tabs.len();
            let target = dst_index.map_or(len.saturating_sub(1), |i| i.min(len.saturating_sub(1)));
            self.reorder_tab(dst_win_id, tab_id, target)?;
            self.windows[src_win_idx].active_tab = tab_id;
            self.active_window = Some(dst_win_id);
            return Ok(src_win_id);
        }

        // Never move tabs into or out of the hidden scratch quick terminal's
        // sole tab slot.
        if dst_is_quick {
            return Err(TopologyError::WindowNotFound(dst_win_id));
        }
        if self.windows[src_win_idx].quick && self.windows[src_win_idx].tabs.len() == 1 {
            let pid = self.windows[src_win_idx].tabs[0].active_pane;
            return Err(TopologyError::AlreadyStandaloneWindow(pid));
        }

        let moved_tab = self.windows[src_win_idx].tabs.remove(src_tab_pos);
        if self.windows[src_win_idx].tabs.is_empty() {
            self.windows.remove(src_win_idx);
        } else if self.windows[src_win_idx].active_tab == tab_id {
            let next_idx = src_tab_pos.min(self.windows[src_win_idx].tabs.len() - 1);
            self.windows[src_win_idx].active_tab = self.windows[src_win_idx].tabs[next_idx].id;
        }

        let dst_win = self
            .window_mut(dst_win_id)
            .expect("dst_win_id verified above");
        let insert_at = dst_index.map_or(dst_win.tabs.len(), |i| i.min(dst_win.tabs.len()));
        dst_win.tabs.insert(insert_at, moved_tab);
        dst_win.active_tab = tab_id;
        self.active_window = Some(dst_win_id);
        Ok(src_win_id)
    }

    /// Tears an entire `TabModel` (including any split tree and ratios) out of
    /// a multi-tab window into a brand-new top-level `WindowModel` without
    /// restarting any shell. Refuses when `tab_id` is already the sole tab in
    /// its window (`AlreadyStandaloneWindow`).
    pub fn detach_tab(&mut self, tab_id: TabId) -> Result<WindowId, TopologyError> {
        let Some((src_win_idx, src_tab_pos)) =
            self.windows.iter().enumerate().find_map(|(wi, w)| {
                w.tabs
                    .iter()
                    .position(|t| t.id == tab_id)
                    .map(|ti| (wi, ti))
            })
        else {
            return Err(TopologyError::TabNotFound(tab_id));
        };
        if self.windows[src_win_idx].quick || self.windows[src_win_idx].tabs.len() <= 1 {
            let pid = self.windows[src_win_idx]
                .tabs
                .get(src_tab_pos)
                .map_or(PaneId(0), |t| t.active_pane);
            return Err(TopologyError::AlreadyStandaloneWindow(pid));
        }
        let menubar_visible = self.windows[src_win_idx].menubar_visible;
        let moved_tab = self.windows[src_win_idx].tabs.remove(src_tab_pos);
        if self.windows[src_win_idx].active_tab == tab_id {
            let next_idx = src_tab_pos.min(self.windows[src_win_idx].tabs.len() - 1);
            self.windows[src_win_idx].active_tab = self.windows[src_win_idx].tabs[next_idx].id;
        }
        let new_win_id = self.alloc_window_id();
        self.windows.push(WindowModel {
            id: new_win_id,
            tabs: vec![moved_tab],
            active_tab: tab_id,
            menubar_visible,
            quick: false,
        });
        self.active_window = Some(new_win_id);
        Ok(new_win_id)
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
                quick: w.quick,
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
            quick_visible: None,
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

    #[test]
    fn set_ratio_at_path_targets_the_right_split_and_clamps() {
        let mut ws = Workspace::new();
        let p1 = ws.reserve_pane_id();
        let (_w, t1) = ws.insert_window_with_pane(p1, PaneEngine::new_headless(20, 5), true);
        let p2 = ws.reserve_pane_id();
        ws.split_pane_with(
            p1,
            SplitAxis::Horizontal,
            p2,
            PaneEngine::new_headless(20, 5),
        )
        .unwrap();
        let p3 = ws.reserve_pane_id();
        ws.split_pane_with(p2, SplitAxis::Vertical, p3, PaneEngine::new_headless(20, 5))
            .unwrap();

        // Root split, then the nested one under `second`.
        assert!(ws.set_split_ratio(t1, &[], 0.3));
        assert!(ws.set_split_ratio(t1, &[true], 0.7));
        let root = &ws.window(_w).unwrap().tabs[0].root;
        match root {
            SplitNode::Split { ratio, second, .. } => {
                assert!((ratio - 0.3).abs() < 1e-9);
                match second.as_ref() {
                    SplitNode::Split { ratio: r2, .. } => assert!((r2 - 0.7).abs() < 1e-9),
                    _ => panic!("expected nested split"),
                }
            }
            _ => panic!("expected root split"),
        }

        // Clamped so a pane can never be dragged away entirely.
        ws.set_split_ratio(t1, &[], 0.0);
        ws.set_split_ratio(t1, &[true], 1.0);
        match &ws.window(_w).unwrap().tabs[0].root {
            SplitNode::Split { ratio, second, .. } => {
                assert!(*ratio >= 0.05);
                match second.as_ref() {
                    SplitNode::Split { ratio: r2, .. } => assert!(*r2 <= 0.95),
                    _ => panic!(),
                }
            }
            _ => panic!(),
        }

        // A path that lands on a leaf, or a missing tab, is reported false.
        assert!(!ws.set_split_ratio(t1, &[false], 0.5));
        assert!(!ws.set_split_ratio(t1, &[true, true, true], 0.5));
        assert!(!ws.set_split_ratio(TabId(999), &[], 0.5));
    }

    #[test]
    fn ratio_survives_detach_move_and_further_splits() {
        let mut ws = Workspace::new();
        let p1 = ws.reserve_pane_id();
        let (w1, t1) = ws.insert_window_with_pane(p1, PaneEngine::new_headless(20, 5), true);
        let p2 = ws.reserve_pane_id();
        ws.split_pane_with(
            p1,
            SplitAxis::Horizontal,
            p2,
            PaneEngine::new_headless(20, 5),
        )
        .unwrap();
        ws.set_split_ratio(t1, &[], 0.25);

        // A further split elsewhere must not disturb the stored ratio.
        let p3 = ws.reserve_pane_id();
        ws.split_pane_with(p2, SplitAxis::Vertical, p3, PaneEngine::new_headless(20, 5))
            .unwrap();
        let ratio_of = |ws: &Workspace, w: WindowId| match &ws.window(w).unwrap().tabs[0].root {
            SplitNode::Split { ratio, .. } => *ratio,
            _ => panic!("expected split"),
        };
        assert!((ratio_of(&ws, w1) - 0.25).abs() < 1e-9);

        // Detaching a pane keeps the ratio on the source tree.
        let (w2, _t2) = ws.detach_split(p3).unwrap();
        assert!((ratio_of(&ws, w1) - 0.25).abs() < 1e-9);

        // Moving it back likewise.
        ws.move_pane_to_tab(p3, t1, SplitAxis::Vertical).unwrap();
        assert!((ratio_of(&ws, w1) - 0.25).abs() < 1e-9);
        let _ = w2;
    }

    #[test]
    fn reorder_tab_moves_and_clamps() {
        let mut ws = Workspace::new();
        let p1 = ws.reserve_pane_id();
        let (w1, t1) = ws.insert_window_with_pane(p1, PaneEngine::new_headless(20, 5), true);
        let p2 = ws.reserve_pane_id();
        let t2 = ws
            .insert_tab_with_pane(w1, p2, PaneEngine::new_headless(20, 5))
            .unwrap();
        let p3 = ws.reserve_pane_id();
        let t3 = ws
            .insert_tab_with_pane(w1, p3, PaneEngine::new_headless(20, 5))
            .unwrap();

        let ids = |ws: &Workspace| {
            ws.window(w1)
                .unwrap()
                .tabs
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&ws), vec![t1, t2, t3]);

        ws.reorder_tab(w1, t3, 0).unwrap();
        assert_eq!(ids(&ws), vec![t3, t1, t2]);

        ws.reorder_tab(w1, t3, 99).unwrap();
        assert_eq!(ids(&ws), vec![t1, t2, t3], "index clamps to the last slot");

        ws.reorder_tab(w1, t1, 0).unwrap();
        assert_eq!(ids(&ws), vec![t1, t2, t3], "no-op move is fine");

        assert!(ws.reorder_tab(w1, TabId(999), 0).is_err());
        assert!(ws.reorder_tab(WindowId(999), t1, 0).is_err());
    }

    #[test]
    fn move_tab_to_window_redocks_split_trees_and_closes_empty_source_window() {
        let mut ws = Workspace::new();
        let p1 = ws.reserve_pane_id();
        let (w1, t1) = ws.insert_window_with_pane(p1, PaneEngine::new_headless(20, 5), true);
        let p1_split = ws.reserve_pane_id();
        ws.split_pane_with(
            p1,
            SplitAxis::Horizontal,
            p1_split,
            PaneEngine::new_headless(20, 5),
        )
        .unwrap();
        ws.set_split_ratio(t1, &[], 0.35);

        let p2 = ws.reserve_pane_id();
        let t2 = ws
            .insert_tab_with_pane(w1, p2, PaneEngine::new_headless(20, 5))
            .unwrap();

        let p3 = ws.reserve_pane_id();
        let (w2, t3) = ws.insert_window_with_pane(p3, PaneEngine::new_headless(20, 5), true);

        // 1. Redock `t1` (with its 2-pane split + 0.35 ratio) from `w1` to slot 0 of `w2`.
        let src = ws.move_tab_to_window(t1, w2, Some(0)).unwrap();
        assert_eq!(src, w1);
        assert_eq!(ws.window(w1).unwrap().tabs.len(), 1);
        assert_eq!(ws.window(w1).unwrap().active_tab, t2);
        assert_eq!(
            ws.window(w2)
                .unwrap()
                .tabs
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>(),
            vec![t1, t3]
        );
        assert_eq!(ws.window(w2).unwrap().active_tab, t1);
        assert_eq!(
            ws.window(w2).unwrap().tabs[0].root.leaves(),
            vec![p1, p1_split]
        );
        assert!(ws.pane(p1).is_some() && ws.pane(p1_split).is_some());

        // 2. Redock `t2` (the last remaining tab in `w1`) onto the end of `w2` ->
        //    `w1` is now empty and removed automatically.
        let src2 = ws.move_tab_to_window(t2, w2, None).unwrap();
        assert_eq!(src2, w1);
        assert!(ws.window(w1).is_none(), "empty source window must close");
        assert_eq!(
            ws.window(w2)
                .unwrap()
                .tabs
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>(),
            vec![t1, t3, t2]
        );
        assert_eq!(ws.window(w2).unwrap().active_tab, t2);

        // 3. Tear `t1` (with its 2-pane split) back out into a brand-new window,
        //    and verify tearing off the sole tab in that new window is refused.
        let w3 = ws.detach_tab(t1).unwrap();
        assert_ne!(w3, w2);
        assert_eq!(ws.window(w3).unwrap().tabs.len(), 1);
        assert_eq!(ws.window(w3).unwrap().tabs[0].id, t1);
        assert_eq!(
            ws.window(w3).unwrap().tabs[0].root.leaves(),
            vec![p1, p1_split]
        );
        assert!(matches!(
            ws.detach_tab(t1),
            Err(TopologyError::AlreadyStandaloneWindow(_))
        ));
    }

    #[test]
    fn balanced_grid_preserves_pane_order_for_1_to_9_panes() {
        assert!(SplitNode::balanced_grid(&[]).is_none());
        for count in [1usize, 2, 3, 4, 6, 9] {
            let ids: Vec<PaneId> = (1..=count as u64).map(PaneId).collect();
            let grid = SplitNode::balanced_grid(&ids).expect("non-empty grid");
            assert_eq!(grid.leaves(), ids, "leaf order must hold for {count} panes");
        }

        let mut ws = Workspace::new();
        let mut entries = Vec::new();
        for _ in 0..4 {
            let pid = ws.reserve_pane_id();
            entries.push((pid, PaneEngine::new_headless(30, 8)));
        }
        let expected: Vec<PaneId> = entries.iter().map(|(id, _)| *id).collect();
        let (win_id, _tab_id, first) = ws
            .insert_window_with_grid(entries, true)
            .expect("grid window");
        assert_eq!(first, expected[0]);
        assert_eq!(
            ws.window(win_id)
                .unwrap()
                .active_tab_ref()
                .unwrap()
                .root
                .leaves(),
            expected
        );
    }
}
