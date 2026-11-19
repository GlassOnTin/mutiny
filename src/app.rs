use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
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

pub struct AppController {
    pub workspace: RefCell<Workspace>,
    gtk_app: RefCell<Option<gtk4::Application>>,
    ui_windows: RefCell<HashMap<WindowId, MutinyWindow>>,
    ipc_listener: RefCell<Option<UnixListener>>,
    socket_path: RefCell<Option<PathBuf>>,
    agent_listener: RefCell<Option<UnixListener>>,
    agent_socket_path: RefCell<Option<PathBuf>>,
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
        let wins: Vec<MutinyWindow> = self.ui_windows.borrow().values().cloned().collect();
        for w in wins {
            w.refresh_from_config();
        }
        for pid in self.workspace.borrow().pane_ids() {
            self.queue_redraw_pane(pid);
        }
    }

    pub fn attach_gtk_app(&self, app: &gtk4::Application) {
        *self.gtk_app.borrow_mut() = Some(app.clone());
    }

    pub fn start_ipc_listener(&self, path: &Path) -> io::Result<()> {
        let listener = bind_socket(path)?;
        *self.ipc_listener.borrow_mut() = Some(listener);
        *self.socket_path.borrow_mut() = Some(path.to_path_buf());
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

    fn spawn_engine(&self, pane_id: PaneId, cwd: Option<PathBuf>) -> io::Result<PaneEngine> {
        let (cols, rows, scrollback) = {
            let cfg = self.config.borrow();
            (cfg.initial_cols, cfg.initial_rows, cfg.scrollback_lines)
        };
        let size = GridSize::new(cols, rows, 9, 18);
        let shell = self.shell_override.borrow().clone();
        PaneEngine::spawn_pty(size, cwd, shell, pane_id.0, scrollback)
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
        let inherited_cwd = cwd.or_else(|| self.active_cwd(None, None));
        let pane_id = self.workspace.borrow_mut().reserve_pane_id();
        let engine = self.spawn_engine(pane_id, inherited_cwd)?;
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
        let win_id = target_win
            .or_else(|| self.workspace.borrow().active_window_id())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no active window"))?;
        let inherited_cwd = cwd.or_else(|| self.active_cwd(Some(win_id), None));
        let pane_id = self.workspace.borrow_mut().reserve_pane_id();
        let engine = self.spawn_engine(pane_id, inherited_cwd)?;
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
        let anchor = target_pane
            .or_else(|| self.workspace.borrow().active_pane_id())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no active pane"))?;
        let inherited_cwd = cwd.or_else(|| self.active_cwd(None, Some(anchor)));
        let new_pane = self.workspace.borrow_mut().reserve_pane_id();
        let engine = self.spawn_engine(new_pane, inherited_cwd)?;
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

    pub fn close_pane(self: &Rc<Self>, pane_id: PaneId) {
        let src_win = self.workspace.borrow().locate_pane(pane_id).map(|(w, _)| w);
        let _dropped = self.workspace.borrow_mut().close_pane(pane_id);
        if let Some(win_id) = src_win {
            self.reconcile_or_close_ui_window(win_id);
        }
        self.quit_if_empty();
    }

    pub fn handle_window_closed(&self, win_id: WindowId) {
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
                let _ = self.create_tab(active_win, None);
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

    fn ensure_ui_window(self: &Rc<Self>, win_id: WindowId) {
        if self.ui_windows.borrow().contains_key(&win_id) {
            return;
        }
        let Some(app) = self.gtk_app.borrow().clone() else {
            return;
        };
        let ui_win = MutinyWindow::new(&app, win_id, Rc::clone(self));
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
        self.accept_carrier(&self.ipc_listener, ConsentMode::Operator, "ctl");
        let agent_mode = self.agent_mode.get();
        self.accept_carrier(&self.agent_listener, agent_mode, "mcp-agent");
    }

    fn accept_carrier(
        self: &Rc<Self>,
        cell: &RefCell<Option<UnixListener>>,
        carrier: ConsentMode,
        default_label: &str,
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
                Ok((stream, _)) => self.handle_accepted(stream, carrier, default_label),
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
        let peer = PeerIdentity::from_credentials(&cred);

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
    }
}
