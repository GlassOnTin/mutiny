use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::{Rc, Weak};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gtk4::gdk;
use gtk4::prelude::*;

use crate::action::ActionId;
use crate::config::{self, Overrides, RELOAD_CHECK_INTERVAL, ResolvedConfig};
use crate::crypto;
use crate::engine::{
    EngineNotification, GridSize, PaneEngine, SEARCH_LINES_PER_TICK, ScrollbackSearch,
    parse_named_key,
};
use crate::http_mcp::{PAIRING_TIMEOUT, PairingOutcome, PairingRequest};
use crate::ipc::{
    AuditDecision, AuditEntry, CarrierRole, ConsentMode, ConsentOutcome, IpcEnvelope, IpcRequest,
    IpcResponse, PeerIdentity, action_catalog, bind_socket, carrier_permits_peer, consent_outcome,
    default_audit_path, peer_credentials, read_envelope, server_uid, write_response,
};
use crate::mux::{
    self, DISCOVERY_BYTES_PER_TICK, HostState, MAX_CONCURRENT_DISCOVERIES, MAX_DISCOVERY_BYTES,
    MuxHostStatus, MuxInventory, MuxKind, MuxSession,
};
use crate::notify::{
    CompositeNotifier, DBusNotifier, DesktopNotifier, NotificationPriority, OutboundNotification,
    TokenBucket,
};
use crate::peer_client::{
    HavenAuthPrompt, HavenPeerClientHandle, PeerClientEvent, PeerPollConfig,
    classify_present_target, parse_and_validate_peer_url,
};
use crate::peers::{
    self, HavenLimbInventory, InboundPeer, InboundPeerTable, LiveInboundPeer, OutboundLimbStatus,
    OutboundPeerSummary, OutboundPeerTelemetry, PeerAlert, PeerAlertKind, PeerStore, PeersListing,
    unix_now_secs,
};
use crate::session::{self, SessionState};
use crate::topology::{PaneId, SplitAxis, TabId, TopologyError, WindowId, Workspace};
use crate::ui::window::MutinyWindow;

const MAX_AUDIT_ENTRIES: usize = 256;
const MAX_PEER_ALERTS: usize = 64;
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
    pub created_at: Instant,
    pub deadline: Instant,
    pub relayed_to_haven: bool,
    pub remote_request_id: Option<String>,
    pub remote_sandbox_id: Option<String>,
    stream: UnixStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentResolution {
    AllowOnce,
    AllowSession,
    Deny,
}

/// Which session to open, grouped so the shared open path stays readable.
struct SessionRequest<'a> {
    kind: MuxKind,
    name: &'a str,
    host: Option<&'a str>,
}

/// A one-shot multiplexer command and what to do when it succeeds.
struct PendingMuxCommand {
    child: std::process::Child,
    out: Vec<u8>,
    err: Vec<u8>,
    deadline: Instant,
    killed: bool,
    effect: MuxEffect,
    reply: Option<UnixStream>,
}

/// Applied only after the command exits successfully, so a failed rename does
/// not leave mutiny claiming a name the multiplexer never accepted.
enum MuxEffect {
    /// Nothing to rewrite: the panes' clients exit when the session dies, and
    /// the usual exited-pane path closes them. Only the inventory needs a refresh.
    End { kind: MuxKind, name: String },
    Rename {
        kind: MuxKind,
        host: Option<String>,
        old: String,
        new: String,
    },
}

/// A finished discovery job: which target, its sessions or an error, and how
/// long it took.
type DiscoveryOutcome = (
    Option<String>,
    MuxKind,
    Result<Vec<MuxSession>, String>,
    u64,
);

/// One in-flight discovery child. Spawned on the tick (fork+exec is well under
/// a millisecond) and polled with `try_wait`, so nothing slow ever blocks the
/// GUI loop. Threads are deliberately not used: alacritty's `tty::setup_env`
/// mutates the process environment on every pane spawn, so a worker thread
/// reading `environ` concurrently would be unsound.
struct PendingDiscovery {
    host: Option<String>,
    kind: MuxKind,
    child: std::process::Child,
    out: Vec<u8>,
    err: Vec<u8>,
    started: Instant,
    deadline: Instant,
    /// Set once the deadline passed and we sent SIGKILL. The job stays until
    /// `try_wait` reaps it, because `Child::drop` neither kills nor reaps.
    killed: bool,
}

/// One relay carrier for an HTTP peer token. The identity is assigned when the
/// operator configures or pairs the peer, not by anything the remote client sends.
struct HttpPeerCarrier {
    listener: UnixListener,
    path: PathBuf,
    identity: PeerIdentity,
    peer_id: String,
    label: String,
    /// Mode this peer's carrier assigns. Pinned per peer at launch or pairing,
    /// never `Operator`.
    mode: ConsentMode,
}

pub struct PendingInboundPairing {
    pub id: u64,
    pub client_name: String,
    pub remote_addr: SocketAddr,
    pub deadline: Instant,
    reply_tx: std::sync::mpsc::SyncSender<PairingOutcome>,
}

pub struct AppController {
    pub workspace: RefCell<Workspace>,
    gtk_app: RefCell<Option<gtk4::Application>>,
    ui_windows: RefCell<HashMap<WindowId, MutinyWindow>>,
    ipc_listener: RefCell<Option<UnixListener>>,
    socket_path: RefCell<Option<PathBuf>>,
    agent_listener: RefCell<Option<UnixListener>>,
    agent_socket_path: RefCell<Option<PathBuf>>,
    /// One relay carrier per HTTP peer token, each assigning a distinct
    /// token-derived identity. Empty unless the HTTP listener is enabled.
    http_peers: RefCell<Vec<HttpPeerCarrier>>,
    peers_path: RefCell<PathBuf>,
    peer_store: RefCell<PeerStore>,
    inbound_peer_table: Arc<RwLock<InboundPeerTable>>,
    pairing_rx: RefCell<Option<std::sync::mpsc::Receiver<PairingRequest>>>,
    pending_pairing: RefCell<Option<PendingInboundPairing>>,
    next_pairing_id: Cell<u64>,
    allow_headless_pairing: Cell<bool>,
    peer_poll_config: RefCell<PeerPollConfig>,
    peer_client: RefCell<Option<HavenPeerClientHandle>>,
    outbound_statuses: RefCell<HashMap<String, OutboundLimbStatus>>,
    pairing_outbound_label: RefCell<Option<String>>,
    pending_pair_replies: RefCell<HashMap<u64, UnixStream>>,
    pending_handoff_replies: RefCell<HashMap<u64, UnixStream>>,
    pending_present_replies: RefCell<HashMap<u64, UnixStream>>,
    limb_inventories: RefCell<HashMap<String, HavenLimbInventory>>,
    pending_tree_reply: RefCell<Option<UnixStream>>,
    tree_awaiting_limbs: Cell<bool>,
    next_pair_req_id: Cell<u64>,
    consent_replay_cache: RefCell<crypto::GrantReplayCache>,
    haven_auth_prompts: RefCell<HashMap<String, (String, HavenAuthPrompt)>>,
    in_flight_auth_answers: RefCell<HashSet<String>>,
    dismissed_auth_prompts: RefCell<HashSet<(String, String)>>,
    remote_consent_prompts: RefCell<HashMap<String, String>>,
    outbound_rate_limiters: RefCell<HashMap<String, TokenBucket>>,
    peer_alerts: RefCell<VecDeque<PeerAlert>>,
    next_peer_alert_id: Cell<u64>,
    peer_shade_statuses: RefCell<HashMap<String, String>>,
    last_pushed_shade_state: RefCell<(Vec<String>, bool)>,
    force_window_focused_for_test: Cell<Option<bool>>,
    next_peer_sock_seq: Cell<usize>,
    discovery_jobs: RefCell<Vec<PendingDiscovery>>,
    discovery_queue: RefCell<VecDeque<(Option<String>, MuxKind)>>,
    session_cache: RefCell<MuxInventory>,
    /// Bumped whenever the cache changes, so the picker refreshes on change
    /// rather than every 4 ms tick.
    cache_gen: Cell<u64>,
    pending_session_reply: RefCell<Option<UnixStream>>,
    discovery_deadline: Cell<Option<Instant>>,
    /// Every live Sessions submenu (the app menubar with `None`, and each
    /// window's own with `Some(win_id)` so closed windows prune theirs).
    sessions_menus: RefCell<Vec<(Option<WindowId>, gtk4::gio::Menu)>>,
    sessions_menu_gen: Cell<u64>,
    /// Last agent-active state pushed to the window icons, so the icon is only
    /// touched on a transition.
    agent_icon_active: Cell<bool>,
    /// Installed Tier-3 standing policies, and their rolling call history.
    policies: RefCell<Vec<crate::policy::StandingPolicy>>,
    policy_uses: RefCell<std::collections::BTreeMap<u64, VecDeque<u64>>>,
    next_policy_id: Cell<u64>,
    pending_policies: RefCell<Vec<crate::policy::StandingPolicy>>,
    /// Identity of the call currently being dispatched. Set by the consent path
    /// so a handler can bind a proposal to the verified peer, including the
    /// token-derived identity of an HTTP peer rather than this process's pid.
    dispatch_peer: Cell<PeerIdentity>,
    dispatch_carrier: Cell<ConsentMode>,
    dispatch_client_label: RefCell<String>,
    unattached_filter: Cell<bool>,
    /// Whether the quick terminal is currently summoned. The controller owns
    /// this rather than reading GTK, so the toggle is testable headlessly.
    quick_visible: Cell<bool>,
    last_discovery_started: Cell<Option<Instant>>,
    /// In-flight one-shot multiplexer commands (rename). Same tick-polled shape
    /// as discovery: a rename may go over ssh, so it must not block the loop.
    mux_commands: RefCell<Vec<PendingMuxCommand>>,
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
    notifier: RefCell<CompositeNotifier>,
    session_path: RefCell<PathBuf>,
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
        let default_peers_path = config_path
            .parent()
            .map(|p| p.join("peers.json"))
            .unwrap_or_else(peers::peers_path);
        let initial_peers = match peers::load(&default_peers_path) {
            Ok(store) => store,
            Err(err) => {
                eprintln!("warning: peers.json not loaded, starting with empty peer store: {err}");
                PeerStore::default()
            }
        };
        let poll_cfg = PeerPollConfig::default();
        let initial_shade_state = (resolved.inbound_shade_allowlist.clone(), false);
        let initial_client = if initial_peers.outbound_peers.is_empty() {
            None
        } else {
            let client = HavenPeerClientHandle::spawn(
                initial_peers.outbound_peers.clone(),
                poll_cfg.clone(),
            );
            client.set_shade_poll_state(initial_shade_state.0.clone(), initial_shade_state.1);
            Some(client)
        };
        Rc::new(Self {
            workspace: RefCell::new(Workspace::new()),
            gtk_app: RefCell::new(None),
            ui_windows: RefCell::new(HashMap::new()),
            ipc_listener: RefCell::new(None),
            socket_path: RefCell::new(None),
            agent_listener: RefCell::new(None),
            http_peers: RefCell::new(Vec::new()),
            peers_path: RefCell::new(default_peers_path),
            peer_store: RefCell::new(initial_peers),
            inbound_peer_table: Arc::new(RwLock::new(InboundPeerTable::new())),
            pairing_rx: RefCell::new(None),
            pending_pairing: RefCell::new(None),
            next_pairing_id: Cell::new(1),
            allow_headless_pairing: Cell::new(false),
            peer_poll_config: RefCell::new(poll_cfg),
            peer_client: RefCell::new(initial_client),
            outbound_statuses: RefCell::new(HashMap::new()),
            pairing_outbound_label: RefCell::new(None),
            pending_pair_replies: RefCell::new(HashMap::new()),
            pending_handoff_replies: RefCell::new(HashMap::new()),
            pending_present_replies: RefCell::new(HashMap::new()),
            limb_inventories: RefCell::new(HashMap::new()),
            pending_tree_reply: RefCell::new(None),
            tree_awaiting_limbs: Cell::new(false),
            next_pair_req_id: Cell::new(1),
            consent_replay_cache: RefCell::new(crypto::GrantReplayCache::default()),
            haven_auth_prompts: RefCell::new(HashMap::new()),
            in_flight_auth_answers: RefCell::new(HashSet::new()),
            dismissed_auth_prompts: RefCell::new(HashSet::new()),
            remote_consent_prompts: RefCell::new(HashMap::new()),
            outbound_rate_limiters: RefCell::new(HashMap::new()),
            peer_alerts: RefCell::new(VecDeque::new()),
            next_peer_alert_id: Cell::new(1),
            peer_shade_statuses: RefCell::new(HashMap::new()),
            last_pushed_shade_state: RefCell::new(initial_shade_state),
            force_window_focused_for_test: Cell::new(None),
            next_peer_sock_seq: Cell::new(1000),
            discovery_jobs: RefCell::new(Vec::new()),
            discovery_queue: RefCell::new(VecDeque::new()),
            session_cache: RefCell::new(MuxInventory::default()),
            cache_gen: Cell::new(0),
            pending_session_reply: RefCell::new(None),
            discovery_deadline: Cell::new(None),
            sessions_menus: RefCell::new(Vec::new()),
            sessions_menu_gen: Cell::new(u64::MAX),
            agent_icon_active: Cell::new(false),
            policies: RefCell::new(Vec::new()),
            policy_uses: RefCell::new(std::collections::BTreeMap::new()),
            next_policy_id: Cell::new(1),
            pending_policies: RefCell::new(Vec::new()),
            dispatch_peer: Cell::new(PeerIdentity::default()),
            dispatch_carrier: Cell::new(ConsentMode::Operator),
            dispatch_client_label: RefCell::new(String::new()),
            unattached_filter: Cell::new(false),
            quick_visible: Cell::new(false),
            last_discovery_started: Cell::new(None),
            mux_commands: RefCell::new(Vec::new()),
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
            notifier: RefCell::new(CompositeNotifier::new(Rc::new(DBusNotifier))),
            session_path: RefCell::new(session::session_path()),
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

    /// Overrides the session state path. Tests point this at a temp file so
    /// they never touch the user's real state directory.
    pub fn set_session_path(&self, path: PathBuf) {
        *self.session_path.borrow_mut() = path;
    }

    /// Writes the current workspace to the session state file.
    pub fn save_session(&self) -> io::Result<()> {
        let state = session::capture(&self.workspace.borrow());
        session::save(&self.session_path.borrow(), &state)
    }

    /// Loads and restores the session file, if present. Returns the pane count
    /// restored, or None when there was no state file.
    pub fn restore_session_from_file(self: &Rc<Self>) -> Result<Option<usize>, String> {
        match session::load(&self.session_path.borrow())? {
            Some(state) => Ok(Some(self.restore_session(state)?)),
            None => Ok(None),
        }
    }

    /// Restores a saved bundle: window and tab order, split trees and ratios,
    /// working directories, and scrollback. Mutiny-owned shells start fresh;
    /// the no-daemon crash contract stands for them.
    ///
    /// A pane that was attached to a multiplexer re-attaches to that session
    /// instead. Those processes belong to the multiplexer, so this is not
    /// process resurrection by mutiny.
    pub fn restore_session(self: &Rc<Self>, state: SessionState) -> Result<usize, String> {
        let opts = self.config.borrow().sessions.clone();
        let mut engines = Vec::new();
        for sp in &state.panes {
            let attach = match &sp.mux {
                Some(session) => match &session.host {
                    None => Some(mux::attach_argv(session.kind, &session.name, &opts)),
                    Some(host) => Some(
                        mux::remote_attach_argv(host, session.kind, &session.name, &opts)
                            .map_err(|e| format!("pane {}: {e}", sp.id))?,
                    ),
                },
                None => None,
            };
            // A remote session's saved cwd is a path on that host, never a
            // local directory.
            let cwd = match (&sp.mux, attach.is_some()) {
                (Some(session), true) if session.host.is_some() => None,
                _ => sp.cwd.clone(),
            };
            let mut eng = self
                .spawn_engine(sp.id, cwd, attach.clone())
                .map_err(|e| format!("pane {}: {e}", sp.id))?;
            eng.resize(GridSize::new(sp.cols, sp.rows, 9, 18));
            eng.set_title(sp.title.clone());
            if let Some(session) = &sp.mux {
                eng.set_mux_session(Some(session.clone()));
                // The multiplexer owns the screen and will redraw it. Replaying
                // saved text into an attaching pane would corrupt that display.
            } else {
                eng.replay_scrollback(&sp.scrollback);
            }
            engines.push((sp.id, eng));
        }
        let count = engines.len();
        session::install(&mut self.workspace.borrow_mut(), state, engines);
        let ids: Vec<WindowId> = self
            .workspace
            .borrow()
            .windows()
            .iter()
            .map(|w| w.id)
            .collect();
        for id in ids {
            self.ensure_ui_window(id);
            self.sync_ui_window(id);
            if self.workspace.borrow().window(id).is_some_and(|w| w.quick) {
                self.prepare_quick_window(id);
                self.quick_visible.set(false);
                if let Some(ui) = self.ui_windows.borrow().get(&id).cloned() {
                    ui.hide();
                }
            }
        }
        Ok(count)
    }

    /// Replaces the desktop notifier. Tests inject a recorder so forwarding
    /// can be asserted without a session bus.
    pub fn set_notifier(&self, notifier: Rc<dyn DesktopNotifier>) {
        self.notifier.borrow_mut().set_desktop(notifier);
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
        self.sync_shade_poll_state();
        self.apply_config_to_views();
        true
    }

    fn apply_config_to_views(self: &Rc<Self>) {
        let (bg, fg) = {
            let cfg = self.config.borrow();
            (cfg.background, cfg.foreground)
        };
        // Collect first: a temporary borrow in the `for` expression lives for
        // the whole loop, so borrowing mutably inside it would panic.
        let pane_ids = self.workspace.borrow().pane_ids();
        for pid in pane_ids {
            if let Some(eng) = self.workspace.borrow_mut().pane_mut(pid) {
                eng.set_reported_colors(bg, fg);
            }
        }
        let wins: Vec<MutinyWindow> = self.ui_windows.borrow().values().cloned().collect();
        for w in wins {
            w.refresh_from_config();
        }
        for pid in self.workspace.borrow().pane_ids() {
            self.queue_redraw_pane(pid);
        }
    }

    pub fn attach_gtk_app(self: &Rc<Self>, app: &gtk4::Application) {
        *self.gtk_app.borrow_mut() = Some(app.clone());
        // Any model window that exists without a view gets one now. A restore
        // that ran before the application was attached populated the model but
        // could not create widgets, leaving an app with nothing to show, which
        // then exited immediately. Doing it here makes the order safe either
        // way rather than relying on the caller's sequencing.
        let ids: Vec<WindowId> = self
            .workspace
            .borrow()
            .windows()
            .iter()
            .map(|w| w.id)
            .collect();
        for id in ids {
            self.ensure_ui_window(id);
            self.sync_ui_window(id);
        }
    }

    pub fn start_ipc_listener(&self, path: &Path) -> io::Result<()> {
        let listener = bind_socket(path)?;
        *self.ipc_listener.borrow_mut() = Some(listener);
        *self.socket_path.borrow_mut() = Some(path.to_path_buf());
        Ok(())
    }

    /// Binds a relay carrier for one HTTP peer token, assigning the identity
    /// that token maps to. The HTTP carrier connects here after verifying the
    /// token, so the identity is fixed by operator configuration rather than
    /// anything the remote client writes. Grants and audit key on it, which is
    /// what makes two peers distinguishable.
    pub fn start_http_peer_listener(
        &self,
        path: &Path,
        token: &str,
        label: &str,
        mode: ConsentMode,
    ) -> io::Result<()> {
        if mode.carrier_role() != CarrierRole::Peer {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "a network peer carrier can never assign Operator",
            ));
        }
        let listener = bind_socket(path)?;
        let peer_id = format!("cli-{label}");
        self.http_peers.borrow_mut().push(HttpPeerCarrier {
            listener,
            path: path.to_path_buf(),
            identity: PeerIdentity::for_token(token, server_uid()),
            peer_id: peer_id.clone(),
            label: label.to_owned(),
            mode,
        });
        if let Ok(mut table) = self.inbound_peer_table.write() {
            let _ = table.insert(LiveInboundPeer {
                id: peer_id,
                label: label.to_owned(),
                client_name: label.to_owned(),
                token_sha256: crypto::sha256(token.as_bytes()),
                socket: path.to_path_buf(),
                consent_mode: mode,
                ephemeral: true,
                paired_at: unix_now_secs(),
                last_seen_at: None,
            });
        }
        Ok(())
    }

    pub fn inbound_peer_table(&self) -> Arc<RwLock<InboundPeerTable>> {
        Arc::clone(&self.inbound_peer_table)
    }

    pub fn attach_pairing_receiver(&self, rx: std::sync::mpsc::Receiver<PairingRequest>) {
        *self.pairing_rx.borrow_mut() = Some(rx);
    }

    pub fn set_allow_headless_pairing_for_test(&self, allow: bool) {
        self.allow_headless_pairing.set(allow);
    }

    pub fn set_force_window_focused_for_test(&self, focused: Option<bool>) {
        self.force_window_focused_for_test.set(focused);
        self.sync_shade_poll_state();
    }

    pub fn any_window_focused(&self) -> bool {
        if let Some(forced) = self.force_window_focused_for_test.get() {
            return forced;
        }
        self.ui_windows
            .borrow()
            .values()
            .any(|w| w.gtk_window().is_active())
    }

    pub(crate) fn sync_shade_poll_state(&self) {
        let desired = (
            self.config.borrow().inbound_shade_allowlist.clone(),
            self.any_window_focused(),
        );
        if *self.last_pushed_shade_state.borrow() != desired {
            *self.last_pushed_shade_state.borrow_mut() = desired.clone();
            if let Some(client) = self.peer_client.borrow().as_ref() {
                client.set_shade_poll_state(desired.0, desired.1);
            }
        }
    }

    fn force_push_shade_poll_state(&self) {
        let desired = (
            self.config.borrow().inbound_shade_allowlist.clone(),
            self.any_window_focused(),
        );
        *self.last_pushed_shade_state.borrow_mut() = desired.clone();
        if let Some(client) = self.peer_client.borrow().as_ref() {
            client.set_shade_poll_state(desired.0, desired.1);
        }
    }

    pub fn poll_haven_peers_now(&self) {
        if let Some(client) = self.peer_client.borrow().as_ref() {
            client.poll_now();
        }
    }

    pub fn connected_signer_peer(&self) -> Option<(String, String)> {
        let store = self.peer_store.borrow();
        let statuses = self.outbound_statuses.borrow();
        store
            .outbound_peers
            .iter()
            .find(|p| {
                p.signing_secret_b64.is_some()
                    && matches!(
                        statuses.get(&p.id),
                        Some(OutboundLimbStatus::Connected { .. })
                    )
            })
            .map(|p| (p.id.clone(), p.label.clone()))
    }

    pub fn has_connected_haven_signer(&self) -> bool {
        self.connected_signer_peer().is_some()
    }

    pub fn set_allow_lan_peer(&self, allow_lan: bool) {
        self.peer_poll_config.borrow_mut().allow_lan = allow_lan;
        let outbound = self.peer_store.borrow().outbound_peers.clone();
        if !outbound.is_empty() {
            if let Some(mut prev) = self.peer_client.borrow_mut().take() {
                prev.stop();
            }
            let cfg = self.peer_poll_config.borrow().clone();
            *self.peer_client.borrow_mut() = Some(HavenPeerClientHandle::spawn(outbound, cfg));
            self.force_push_shade_poll_state();
        }
    }

    pub fn set_peer_poll_config(&self, cfg: PeerPollConfig) {
        *self.peer_poll_config.borrow_mut() = cfg.clone();
        let outbound = self.peer_store.borrow().outbound_peers.clone();
        if let Some(mut prev) = self.peer_client.borrow_mut().take() {
            prev.stop();
        }
        if !outbound.is_empty() {
            *self.peer_client.borrow_mut() = Some(HavenPeerClientHandle::spawn(outbound, cfg));
            self.force_push_shade_poll_state();
        }
    }

    pub fn set_peers_path(&self, path: PathBuf) -> Result<(), String> {
        let store = peers::load(&path)?;
        *self.peers_path.borrow_mut() = path;
        *self.peer_store.borrow_mut() = store.clone();
        if let Some(mut prev) = self.peer_client.borrow_mut().take() {
            prev.stop();
        }
        if !store.outbound_peers.is_empty() {
            let cfg = self.peer_poll_config.borrow().clone();
            *self.peer_client.borrow_mut() =
                Some(HavenPeerClientHandle::spawn(store.outbound_peers, cfg));
            self.force_push_shade_poll_state();
        }
        Ok(())
    }

    /// Binds per-peer Unix relay sockets for all durable inbound peers loaded
    /// from `peers.json` and registers them in `inbound_peer_table`.
    pub fn bind_durable_inbound_peers(&self, base_agent_sock: &Path) -> io::Result<usize> {
        let durable = self.peer_store.borrow().inbound_peers.clone();
        let mut count = 0usize;
        for peer in durable {
            if peer.consent_mode.carrier_role() != CarrierRole::Peer {
                continue;
            }
            let Ok(sha_bytes) = crypto::hex_decode_32(&peer.token_sha256) else {
                continue;
            };
            let sock = self.allocate_peer_relay_socket(Some(base_agent_sock));
            let listener = bind_socket(&sock)?;
            self.http_peers.borrow_mut().push(HttpPeerCarrier {
                listener,
                path: sock.clone(),
                identity: PeerIdentity::for_token_sha256(&peer.token_sha256, server_uid()),
                peer_id: peer.id.clone(),
                label: peer.label.clone(),
                mode: peer.consent_mode,
            });
            if let Ok(mut table) = self.inbound_peer_table.write() {
                let _ = table.insert(LiveInboundPeer {
                    id: peer.id,
                    label: peer.label,
                    client_name: peer.client_name,
                    token_sha256: sha_bytes,
                    socket: sock,
                    consent_mode: peer.consent_mode,
                    ephemeral: false,
                    paired_at: peer.paired_at,
                    last_seen_at: peer.last_seen_at,
                });
            }
            count += 1;
        }
        Ok(count)
    }

    fn allocate_peer_relay_socket(&self, base_override: Option<&Path>) -> PathBuf {
        let seq = self.next_peer_sock_seq.get();
        self.next_peer_sock_seq.set(seq + 1);
        let base_opt = base_override
            .map(Path::to_path_buf)
            .or_else(|| self.agent_socket_path.borrow().clone())
            .or_else(|| self.socket_path.borrow().clone());
        match base_opt {
            Some(base) => {
                let stem = base
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| String::from("mutiny-agent"));
                let name = format!("{stem}-paired-{seq}.sock");
                match base.parent() {
                    Some(parent) => parent.join(name),
                    None => PathBuf::from(name),
                }
            }
            None => std::env::temp_dir()
                .join(format!("mutiny-paired-{}-{seq}.sock", std::process::id())),
        }
    }

    pub fn pending_pairing_prompt(&self) -> Option<(u64, String, SocketAddr)> {
        let guard = self.pending_pairing.borrow();
        let p = guard.as_ref()?;
        Some((p.id, p.client_name.clone(), p.remote_addr))
    }

    pub fn resolve_inbound_pairing(
        &self,
        id: u64,
        approved_mode: Option<ConsentMode>,
    ) -> Result<Option<String>, String> {
        let maybe_item = {
            let mut slot = self.pending_pairing.borrow_mut();
            if slot.as_ref().is_some_and(|p| p.id == id) {
                slot.take()
            } else {
                None
            }
        };
        let Some(item) = maybe_item else {
            return Err(format!("no pending pairing prompt {id}"));
        };

        let Some(mode) = approved_mode else {
            let _ = item.reply_tx.send(PairingOutcome::Denied);
            self.refresh_all_window_chromes();
            return Ok(None);
        };

        if mode.carrier_role() != CarrierRole::Peer {
            let _ = item.reply_tx.send(PairingOutcome::Denied);
            self.refresh_all_window_chromes();
            return Err(String::from(
                "inbound HTTP peer can never be assigned Operator consent mode",
            ));
        }

        let token = match crypto::generate_pairing_token() {
            Ok(t) => t,
            Err(err) => {
                let _ = item.reply_tx.send(PairingOutcome::Denied);
                self.refresh_all_window_chromes();
                return Err(format!("failed to generate pairing token: {err}"));
            }
        };
        let sha_bytes = crypto::sha256(token.as_bytes());
        let sha_hex = crypto::hex_encode(&sha_bytes);

        let mut raw_id = [0u8; 4];
        let _ = crypto::fill_random(&mut raw_id);
        let peer_id = format!("in-{}", crypto::hex_encode(&raw_id));

        let label = self.deduplicate_inbound_label(&item.client_name);
        let sock = self.allocate_peer_relay_socket(None);
        let listener = match bind_socket(&sock) {
            Ok(l) => l,
            Err(err) => {
                let _ = item.reply_tx.send(PairingOutcome::Denied);
                self.refresh_all_window_chromes();
                return Err(format!("failed to bind relay socket: {err}"));
            }
        };

        let now = unix_now_secs();
        let record = InboundPeer {
            id: peer_id.clone(),
            label: label.clone(),
            client_name: item.client_name.clone(),
            token_sha256: sha_hex.clone(),
            consent_mode: mode,
            paired_at: now,
            last_seen_at: Some(now),
        };
        {
            let mut store = self.peer_store.borrow_mut();
            store.inbound_peers.push(record);
            if let Err(err) = peers::save(&self.peers_path.borrow(), &store) {
                store.inbound_peers.pop();
                let _ = std::fs::remove_file(&sock);
                let _ = item.reply_tx.send(PairingOutcome::Denied);
                self.refresh_all_window_chromes();
                return Err(format!("failed to persist peers.json: {err}"));
            }
        }

        self.http_peers.borrow_mut().push(HttpPeerCarrier {
            listener,
            path: sock.clone(),
            identity: PeerIdentity::for_token_sha256(&sha_hex, server_uid()),
            peer_id: peer_id.clone(),
            label: label.clone(),
            mode,
        });
        if let Ok(mut table) = self.inbound_peer_table.write() {
            let _ = table.insert(LiveInboundPeer {
                id: peer_id.clone(),
                label,
                client_name: item.client_name,
                token_sha256: sha_bytes,
                socket: sock,
                consent_mode: mode,
                ephemeral: false,
                paired_at: now,
                last_seen_at: Some(now),
            });
        }

        let _ = item.reply_tx.send(PairingOutcome::Approved {
            peer_id: peer_id.clone(),
            plaintext_token: token,
        });
        self.refresh_all_window_chromes();
        Ok(Some(peer_id))
    }

    fn deduplicate_inbound_label(&self, desired: &str) -> String {
        let base_raw: String = desired
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let base = base_raw.trim_matches('-');
        let stem = if base.is_empty() { "peer" } else { base };
        let taken = |candidate: &str| -> bool {
            let in_store = self
                .peer_store
                .borrow()
                .inbound_peers
                .iter()
                .any(|p| p.label == candidate);
            let in_table = self
                .inbound_peer_table
                .read()
                .map(|t| t.entries().iter().any(|e| e.label == candidate))
                .unwrap_or(false);
            in_store || in_table
        };
        if !taken(stem) {
            return stem.to_owned();
        }
        for n in 2..1000 {
            let cand = format!("{stem}-{n}");
            if !taken(&cand) {
                return cand;
            }
        }
        format!("{stem}-{}", unix_now_secs())
    }

    pub fn list_peers(&self) -> PeersListing {
        let mut inbound = self
            .inbound_peer_table
            .read()
            .map(|t| t.summaries())
            .unwrap_or_default();
        let store = self.peer_store.borrow();
        for p in &store.inbound_peers {
            if !inbound.iter().any(|s| s.id == p.id || s.label == p.label) {
                inbound.push(peers::InboundPeerSummary {
                    id: p.id.clone(),
                    label: p.label.clone(),
                    client_name: p.client_name.clone(),
                    consent_mode: p.consent_mode,
                    ephemeral: false,
                    paired_at: p.paired_at,
                    last_seen_at: p.last_seen_at,
                });
            }
        }
        let statuses = self.outbound_statuses.borrow();
        let limiters = self.outbound_rate_limiters.borrow();
        let alerts = self.peer_alerts.borrow();
        let shade_statuses = self.peer_shade_statuses.borrow();
        let mut telemetry = HashMap::with_capacity(store.outbound_peers.len());
        for p in &store.outbound_peers {
            let unread = alerts
                .iter()
                .filter(|a| a.unread && (a.peer_id == p.id || a.peer_label == p.label))
                .count();
            telemetry.insert(
                p.id.clone(),
                OutboundPeerTelemetry {
                    rate_limited_count: limiters
                        .get(&p.id)
                        .map(|b| b.rate_limited_count())
                        .unwrap_or(0),
                    unread_alert_count: unread,
                    shade_status: shade_statuses.get(&p.id).cloned(),
                },
            );
        }
        peers::build_listing(&inbound, &store.outbound_peers, &statuses, &telemetry)
    }

    pub fn list_peer_alerts(&self) -> Vec<PeerAlert> {
        self.peer_alerts.borrow().iter().rev().cloned().collect()
    }

    pub fn unread_peer_alert_count(&self) -> usize {
        self.peer_alerts
            .borrow()
            .iter()
            .filter(|a| a.unread)
            .count()
    }

    pub fn clear_peer_alerts(&self) {
        let mut changed = false;
        for alert in self.peer_alerts.borrow_mut().iter_mut() {
            if alert.unread {
                alert.unread = false;
                changed = true;
            }
        }
        if changed {
            self.refresh_all_window_chromes();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_peer_alert(
        &self,
        peer_id: String,
        peer_label: String,
        kind: PeerAlertKind,
        package_name: Option<String>,
        title: String,
        body: String,
        timestamp_ms: u64,
        unread: bool,
    ) {
        let id = self.next_peer_alert_id.get();
        self.next_peer_alert_id.set(id + 1);
        let mut ring = self.peer_alerts.borrow_mut();
        if ring.len() >= MAX_PEER_ALERTS {
            ring.pop_front();
        }
        ring.push_back(PeerAlert {
            id,
            peer_id,
            peer_label,
            kind,
            package_name,
            title,
            body,
            timestamp_ms,
            unread,
        });
    }

    fn mark_peer_alerts_read_by_kind(&self, peer_id: &str, kind: PeerAlertKind) -> bool {
        let mut changed = false;
        for alert in self.peer_alerts.borrow_mut().iter_mut() {
            if alert.unread && alert.peer_id == peer_id && alert.kind == kind {
                alert.unread = false;
                changed = true;
            }
        }
        changed
    }

    fn is_pane_focused_by_operator(&self, pid: PaneId) -> bool {
        if !self.any_window_focused() {
            return false;
        }
        let ws = self.workspace.borrow();
        let Some((win_id, tab_id)) = ws.locate_pane(pid) else {
            return false;
        };
        let win_focused = match self.force_window_focused_for_test.get() {
            Some(true) => ws.active_window_id() == Some(win_id),
            Some(false) => false,
            None => self
                .ui_windows
                .borrow()
                .get(&win_id)
                .is_some_and(|w| w.gtk_window().is_active()),
        };
        win_focused
            && ws
                .window(win_id)
                .is_some_and(|wm| wm.active_tab == tab_id && wm.active_pane() == Some(pid))
    }

    fn dispatch_outbound_notification(&self, event: &OutboundNotification) {
        let relay_mode = self.config.borrow().peer_relay_mode;
        let any_focused = self.any_window_focused();
        self.notifier
            .borrow()
            .dispatch("mutiny", event, relay_mode, any_focused, |ev| {
                let connected_ids: Vec<String> = self
                    .outbound_statuses
                    .borrow()
                    .iter()
                    .filter_map(|(id, st)| match st {
                        OutboundLimbStatus::Connected { .. } => Some(id.clone()),
                        _ => None,
                    })
                    .collect();
                if connected_ids.is_empty() {
                    return;
                }
                let now = Instant::now();
                let client_guard = self.peer_client.borrow();
                let Some(client) = client_guard.as_ref() else {
                    return;
                };
                let mut limiters = self.outbound_rate_limiters.borrow_mut();
                for pid in connected_ids {
                    let bucket = limiters
                        .entry(pid.clone())
                        .or_insert_with(|| TokenBucket::haven_default(now));
                    if bucket.try_acquire(now) {
                        let _ = client.raise_notification(
                            pid,
                            ev.title.clone(),
                            ev.body.clone(),
                            ev.priority.as_str().to_owned(),
                        );
                    }
                }
            });
    }

    pub fn pairing_outbound_label(&self) -> Option<String> {
        self.pairing_outbound_label.borrow().clone()
    }

    /// Computes `(pill_label, tooltip)` for the HeaderBar Limb Status Pill, or
    /// `None` when zero inbound/outbound peers exist and no outbound pairing is
    /// in flight.
    pub fn limb_pill_summary(&self) -> Option<(String, String)> {
        if let Some(lbl) = self.pairing_outbound_label.borrow().clone() {
            return Some((
                String::from("◐ pairing…"),
                format!("Pairing outbound Haven limb '{lbl}'…"),
            ));
        }
        let total_unread = self.unread_peer_alert_count();
        let listing = self.list_peers();
        if let Some(primary) = listing
            .outbound
            .iter()
            .find(|p| matches!(p.status, OutboundLimbStatus::Connected { .. }))
            .or_else(|| listing.outbound.first())
        {
            return match &primary.status {
                OutboundLimbStatus::Connected {
                    label,
                    remote_version,
                    rtt_ms,
                    ..
                } => {
                    let ver = remote_version
                        .as_deref()
                        .map(|v| format!(" v{v}"))
                        .unwrap_or_default();
                    let signer = if primary.has_signing_key {
                        ", signer enrolled"
                    } else {
                        ""
                    };
                    if total_unread > 0 {
                        Some((
                            format!("!haven:{label} ({total_unread})"),
                            format!(
                                "{total_unread} unread Haven alert(s) on '{label}'{ver} ({rtt_ms} ms RTT{signer})"
                            ),
                        ))
                    } else {
                        Some((
                            format!("● haven:{label}"),
                            format!(
                                "Haven limb '{label}'{ver} connected ({rtt_ms} ms RTT{signer})"
                            ),
                        ))
                    }
                }
                OutboundLimbStatus::Pairing { label } => Some((
                    String::from("◐ pairing…"),
                    format!("Pairing Haven limb '{label}'…"),
                )),
                OutboundLimbStatus::Offline {
                    label, last_error, ..
                } => {
                    let detail = last_error.as_deref().unwrap_or("unreachable");
                    if total_unread > 0 {
                        Some((
                            format!("◌ !haven:{label} ({total_unread})"),
                            format!(
                                "Haven limb '{label}' offline ({total_unread} unread alert(s)): {detail}"
                            ),
                        ))
                    } else {
                        Some((
                            format!("◌ haven:{label}"),
                            format!("Haven limb '{label}' offline: {detail}"),
                        ))
                    }
                }
            };
        }
        if !listing.inbound.is_empty() {
            let first = &listing.inbound[0];
            if total_unread > 0 {
                let alert_label = self
                    .peer_alerts
                    .borrow()
                    .iter()
                    .rev()
                    .find(|a| a.unread)
                    .map(|a| a.peer_label.clone())
                    .unwrap_or_else(|| first.label.clone());
                return Some((
                    format!("!haven:{alert_label} ({total_unread})"),
                    format!("{total_unread} unread peer alert(s) from '{alert_label}'"),
                ));
            }
            if listing.inbound.len() == 1 {
                return Some((
                    format!("● limb:{}", first.label),
                    format!(
                        "Inbound paired peer '{}' ({:?})",
                        first.label, first.consent_mode
                    ),
                ));
            }
            return Some((
                format!("● limbs:{}", listing.inbound.len()),
                format!("{} inbound paired peers", listing.inbound.len()),
            ));
        }
        if total_unread > 0 {
            let alert_label = self
                .peer_alerts
                .borrow()
                .iter()
                .rev()
                .find(|a| a.unread)
                .map(|a| a.peer_label.clone())
                .unwrap_or_else(|| String::from("peer"));
            return Some((
                format!("!haven:{alert_label} ({total_unread})"),
                format!("{total_unread} unread peer alert(s)"),
            ));
        }
        None
    }

    pub fn begin_pair_haven(
        &self,
        label: String,
        url: String,
        token: Option<String>,
        enroll_signer: bool,
        reply: Option<UnixStream>,
    ) -> Result<(), String> {
        let clean_label = mux::sanitize_text(label.trim(), 48);
        if clean_label.is_empty() {
            return Err(String::from("peer label must not be empty"));
        }
        let allow_lan = self.peer_poll_config.borrow().allow_lan;
        parse_and_validate_peer_url(&url, allow_lan)?;

        if self.peer_client.borrow().is_none() {
            let initial = self.peer_store.borrow().outbound_peers.clone();
            let cfg = self.peer_poll_config.borrow().clone();
            *self.peer_client.borrow_mut() = Some(HavenPeerClientHandle::spawn(initial, cfg));
            self.force_push_shade_poll_state();
        }
        let req_id = self.next_pair_req_id.get();
        self.next_pair_req_id.set(req_id + 1);
        *self.pairing_outbound_label.borrow_mut() = Some(clean_label.clone());
        if let Some(stream) = reply {
            self.pending_pair_replies
                .borrow_mut()
                .insert(req_id, stream);
        }
        let send_res = self
            .peer_client
            .borrow()
            .as_ref()
            .expect("peer client spawned")
            .pair_outbound(req_id, clean_label, url, token, enroll_signer);
        if let Err(err) = send_res {
            *self.pairing_outbound_label.borrow_mut() = None;
            self.pending_pair_replies.borrow_mut().remove(&req_id);
            self.refresh_all_window_chromes();
            return Err(err);
        }
        self.refresh_all_window_chromes();
        Ok(())
    }

    pub fn begin_enroll_haven_signer(
        &self,
        id_or_label: String,
        reply: Option<UnixStream>,
    ) -> Result<(), String> {
        let target = id_or_label.trim();
        if target.is_empty() {
            return Err(String::from("peer id or label must not be empty"));
        }
        let exists = self
            .peer_store
            .borrow()
            .outbound_peers
            .iter()
            .any(|p| p.id == target || p.label == target);
        if !exists {
            return Err(format!("no paired outbound Haven peer matching {target:?}"));
        }
        if self.peer_client.borrow().is_none() {
            let initial = self.peer_store.borrow().outbound_peers.clone();
            let cfg = self.peer_poll_config.borrow().clone();
            *self.peer_client.borrow_mut() = Some(HavenPeerClientHandle::spawn(initial, cfg));
            self.force_push_shade_poll_state();
        }
        let req_id = self.next_pair_req_id.get();
        self.next_pair_req_id.set(req_id + 1);
        if let Some(stream) = reply {
            self.pending_pair_replies
                .borrow_mut()
                .insert(req_id, stream);
        }
        let send_res = self
            .peer_client
            .borrow()
            .as_ref()
            .expect("peer client spawned")
            .enroll_signer(req_id, target.to_owned());
        if let Err(err) = send_res {
            self.pending_pair_replies.borrow_mut().remove(&req_id);
            return Err(err);
        }
        Ok(())
    }

    pub fn revoke_peer(&self, id_or_label: &str) -> Result<(), String> {
        let target = id_or_label.trim();
        if target.is_empty() {
            return Err(String::from("peer id or label must not be empty"));
        }

        let mut removed_any = false;
        let removed_live = self
            .inbound_peer_table
            .write()
            .map(|mut t| t.revoke(target))
            .unwrap_or_default();
        if !removed_live.is_empty() {
            removed_any = true;
        }
        let removed_ids: Vec<String> = removed_live.iter().map(|e| e.id.clone()).collect();
        let removed_labels: Vec<String> = removed_live.iter().map(|e| e.label.clone()).collect();

        // Tear down matching per-peer Unix relay sockets and purge any standing
        // grants/policies keyed on that peer's token identity.
        let mut revoked_identities = Vec::new();
        {
            let mut carriers = self.http_peers.borrow_mut();
            let mut kept = Vec::with_capacity(carriers.len());
            for carrier in carriers.drain(..) {
                if carrier.peer_id == target
                    || carrier.label == target
                    || removed_ids.contains(&carrier.peer_id)
                {
                    let _ = std::fs::remove_file(&carrier.path);
                    revoked_identities.push(carrier.identity);
                    removed_any = true;
                } else {
                    kept.push(carrier);
                }
            }
            *carriers = kept;
        }
        for ident in revoked_identities {
            self.session_approved_clients.borrow_mut().remove(&ident);
            self.session_grant_labels.borrow_mut().remove(&ident);
            self.policies.borrow_mut().retain(|p| p.peer != ident);
            self.pending_policies
                .borrow_mut()
                .retain(|p| p.peer != ident);
        }

        let mut store_changed = false;
        let mut removed_outbound_ids = Vec::new();
        {
            let mut store = self.peer_store.borrow_mut();
            let in_before = store.inbound_peers.len();
            store
                .inbound_peers
                .retain(|p| p.id != target && p.label != target);
            if store.inbound_peers.len() != in_before {
                removed_any = true;
                store_changed = true;
            }
            let mut kept_out = Vec::with_capacity(store.outbound_peers.len());
            for p in store.outbound_peers.drain(..) {
                if p.id == target || p.label == target {
                    removed_outbound_ids.push(p.id);
                    removed_any = true;
                    store_changed = true;
                } else {
                    kept_out.push(p);
                }
            }
            store.outbound_peers = kept_out;
            if store_changed {
                peers::save(&self.peers_path.borrow(), &store)
                    .map_err(|e| format!("failed to save peers.json: {e}"))?;
            }
        }

        for out_id in &removed_outbound_ids {
            self.outbound_statuses.borrow_mut().remove(out_id);
            self.limb_inventories.borrow_mut().remove(out_id);
            self.outbound_rate_limiters.borrow_mut().remove(out_id);
            self.peer_shade_statuses.borrow_mut().remove(out_id);
            self.remote_consent_prompts.borrow_mut().remove(out_id);
            self.haven_auth_prompts.borrow_mut().remove(out_id);
            self.in_flight_auth_answers.borrow_mut().remove(out_id);
            self.dismissed_auth_prompts
                .borrow_mut()
                .retain(|(pid, _)| pid != out_id);
        }
        if !removed_outbound_ids.is_empty() {
            self.bump_cache_gen();
            self.refresh_session_pickers();
        }
        self.peer_alerts.borrow_mut().retain(|a| {
            !removed_outbound_ids.contains(&a.peer_id)
                && a.peer_id != target
                && a.peer_label != target
                && !removed_labels.contains(&a.peer_label)
        });
        if !removed_outbound_ids.is_empty()
            && let Some(client) = self.peer_client.borrow().as_ref()
        {
            let remaining = self.peer_store.borrow().outbound_peers.clone();
            let _ = client.set_peers(remaining);
        }

        if !removed_any {
            return Err(format!("no paired peer matching {target:?}"));
        }
        self.refresh_all_window_chromes();
        Ok(())
    }

    pub fn call_outbound_haven_tool(
        &self,
        peer_id_or_label: &str,
        tool_name: &str,
        arguments: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let guard = self.peer_client.borrow();
        let Some(client) = guard.as_ref() else {
            return Err(String::from("no outbound Haven peer client active"));
        };
        client.call_tool(peer_id_or_label, tool_name, arguments, timeout)
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
        for peer in self.http_peers.borrow_mut().drain(..) {
            let _ = std::fs::remove_file(peer.path);
        }
        if let Some(pending) = self.pending_pairing.borrow_mut().take() {
            let _ = pending.reply_tx.send(PairingOutcome::Denied);
        }
        for (_, mut stream) in self.pending_pair_replies.borrow_mut().drain() {
            let _ = write_response(&mut stream, &IpcResponse::err("server shutting down"));
        }
        for (_, mut stream) in self.pending_handoff_replies.borrow_mut().drain() {
            let _ = write_response(&mut stream, &IpcResponse::err("server shutting down"));
        }
        for (_, mut stream) in self.pending_present_replies.borrow_mut().drain() {
            let _ = write_response(&mut stream, &IpcResponse::err("server shutting down"));
        }
        if let Some(mut stream) = self.pending_tree_reply.borrow_mut().take() {
            let _ = write_response(&mut stream, &IpcResponse::err("server shutting down"));
        }
        self.tree_awaiting_limbs.set(false);
        if let Some(mut client) = self.peer_client.borrow_mut().take() {
            client.stop();
        }
        self.shutdown_discoveries();
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

    pub fn pending_consent_prompt(
        &self,
    ) -> Option<(u64, String, String, String, crate::action::ConsentTier)> {
        let guard = self.pending_consents.borrow();
        let front = guard.front()?;
        Some((
            front.id,
            front.client.clone(),
            front.request.verb_name().to_owned(),
            front.request.redacted_summary(),
            front.request.consent_tier(),
        ))
    }

    pub fn consent_haven_relay_state(&self, consent_id: u64) -> (bool, bool) {
        let already_relayed = self
            .pending_consents
            .borrow()
            .iter()
            .find(|c| c.id == consent_id)
            .is_some_and(|c| c.relayed_to_haven);
        (self.has_connected_haven_signer(), already_relayed)
    }

    pub fn relay_consent_to_haven(&self, consent_id: u64) -> Result<(), String> {
        let Some((peer_id, _)) = self.connected_signer_peer() else {
            return Err(String::from("no connected Haven signer available"));
        };
        let mut raw_id = [0u8; 16];
        crypto::fill_random(&mut raw_id)
            .map_err(|e| format!("failed to generate CSPRNG request_id: {e}"))?;
        let request_id = format!("mty-consent-{}", crypto::hex_encode(&raw_id));

        let dispatch_info = {
            let mut queue = self.pending_consents.borrow_mut();
            let Some(item) = queue.iter_mut().find(|c| c.id == consent_id) else {
                return Err(String::from("consent prompt is no longer pending"));
            };
            if item.relayed_to_haven {
                return Ok(());
            }
            let verb = item.request.verb_name();
            let tier = item.request.consent_tier();
            let tier_slug = match tier {
                crate::action::ConsentTier::Observe => "observe",
                crate::action::ConsentTier::Act => "act",
                crate::action::ConsentTier::PolicyChange => "policy-change",
                crate::action::ConsentTier::Irreversible => "irreversible",
            };
            let clean_caller: String = mux::sanitize_text(&item.client, 48)
                .chars()
                .map(|c| if c.is_whitespace() { '_' } else { c })
                .collect();
            let caller_slug = if clean_caller.is_empty() {
                String::from("peer")
            } else {
                clean_caller
            };
            let sandbox_id = format!("mutiny:{verb}:{tier_slug}:{caller_slug}");
            let tier_tag = match tier {
                crate::action::ConsentTier::Irreversible => "Irreversible",
                crate::action::ConsentTier::PolicyChange => "PolicyChange",
                crate::action::ConsentTier::Act => "Act",
                crate::action::ConsentTier::Observe => "Observe",
            };
            let detail = mux::sanitize_text(&item.request.redacted_summary(), 96);
            let reason = if detail.is_empty() {
                format!("[{tier_tag}] Peer '{caller_slug}' requests {verb}. Approve?")
            } else {
                format!("[{tier_tag}] Peer '{caller_slug}' requests {verb} ({detail}). Approve?")
            };
            item.relayed_to_haven = true;
            item.remote_request_id = Some(request_id.clone());
            item.remote_sandbox_id = Some(sandbox_id.clone());
            item.deadline = item.deadline.max(Instant::now() + Duration::from_secs(68));
            (sandbox_id, reason)
        };

        let send_res = match self.peer_client.borrow().as_ref() {
            Some(client) => client.request_consent(
                consent_id,
                peer_id,
                request_id,
                dispatch_info.0,
                String::from("mutiny.local"),
                1,
                String::from("https"),
                dispatch_info.1,
            ),
            None => Err(String::from("no outbound Haven peer client active")),
        };
        if let Err(err) = send_res {
            if let Some(item) = self
                .pending_consents
                .borrow_mut()
                .iter_mut()
                .find(|c| c.id == consent_id)
            {
                item.relayed_to_haven = false;
                item.remote_request_id = None;
                item.remote_sandbox_id = None;
            }
            return Err(err);
        }
        self.refresh_all_window_chromes();
        Ok(())
    }

    fn poll_unfocused_consent_relay(&self) {
        let (relay_enabled, grace_ms) = {
            let cfg = self.config.borrow();
            (cfg.relay_unfocused_consent, cfg.unfocused_grace_ms)
        };
        if !relay_enabled || self.any_window_focused() || !self.has_connected_haven_signer() {
            return;
        }
        let grace = Duration::from_millis(grace_ms);
        let due_ids: Vec<u64> = self
            .pending_consents
            .borrow()
            .iter()
            .filter(|c| !c.relayed_to_haven && c.created_at.elapsed() >= grace)
            .map(|c| c.id)
            .collect();
        for cid in due_ids {
            let _ = self.relay_consent_to_haven(cid);
        }
    }

    pub(crate) fn active_haven_auth_prompt(&self) -> Option<(String, String, HavenAuthPrompt)> {
        let map = self.haven_auth_prompts.borrow();
        let dismissed = self.dismissed_auth_prompts.borrow();
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for peer_id in keys {
            if let Some((peer_label, prompt)) = map.get(peer_id)
                && !dismissed.contains(&(peer_id.clone(), prompt.profile_id.clone()))
            {
                return Some((peer_id.clone(), peer_label.clone(), prompt.clone()));
            }
        }
        None
    }

    /// Triggered exclusively from physical GTK4 widget interaction on the
    /// Haven Auth Bar. Never reachable over IPC, CLI, or MCP.
    pub(crate) fn submit_haven_auth_prompt(
        &self,
        peer_id: &str,
        profile_id: &str,
        password: String,
        remember_password: bool,
    ) -> Result<(), String> {
        let matches_active = self
            .haven_auth_prompts
            .borrow()
            .get(peer_id)
            .is_some_and(|(_, p)| p.profile_id == profile_id);
        if !matches_active {
            return Err(String::from("auth prompt is no longer pending"));
        }
        self.haven_auth_prompts.borrow_mut().remove(peer_id);
        self.dismissed_auth_prompts
            .borrow_mut()
            .remove(&(peer_id.to_owned(), profile_id.to_owned()));
        self.mark_peer_alerts_read_by_kind(peer_id, PeerAlertKind::AuthPrompt);
        self.in_flight_auth_answers
            .borrow_mut()
            .insert(peer_id.to_owned());
        let res = match self.peer_client.borrow().as_ref() {
            Some(client) => {
                client.answer_auth_prompt(peer_id.to_owned(), password, remember_password)
            }
            None => Err(String::from("no outbound Haven peer client active")),
        };
        if res.is_err() {
            self.in_flight_auth_answers.borrow_mut().remove(peer_id);
        }
        self.refresh_all_window_chromes();
        res
    }

    pub(crate) fn dismiss_haven_auth_prompt(&self, peer_id: &str, profile_id: &str) {
        self.dismissed_auth_prompts
            .borrow_mut()
            .insert((peer_id.to_owned(), profile_id.to_owned()));
        self.mark_peer_alerts_read_by_kind(peer_id, PeerAlertKind::AuthPrompt);
        self.refresh_all_window_chromes();
    }

    pub(crate) fn trigger_insert_peer_totp(
        &self,
        target_window: Option<WindowId>,
        target_pane: PaneId,
    ) -> Result<(), String> {
        let peer_id = self
            .outbound_statuses
            .borrow()
            .iter()
            .find_map(|(id, st)| match st {
                OutboundLimbStatus::Connected { .. } => Some(id.clone()),
                _ => None,
            })
            .or_else(|| {
                self.peer_store
                    .borrow()
                    .outbound_peers
                    .first()
                    .map(|p| p.id.clone())
            })
            .ok_or_else(|| String::from("no paired outbound Haven peer"))?;

        if self.peer_client.borrow().is_none() {
            let initial = self.peer_store.borrow().outbound_peers.clone();
            let cfg = self.peer_poll_config.borrow().clone();
            *self.peer_client.borrow_mut() = Some(HavenPeerClientHandle::spawn(initial, cfg));
            self.force_push_shade_poll_state();
        }
        self.peer_client
            .borrow()
            .as_ref()
            .expect("peer client spawned")
            .list_totp_secrets(peer_id, target_pane, target_window)
    }

    pub(crate) fn request_haven_totp_code(
        &self,
        peer_id: String,
        totp_secret_id: String,
        target_pane: PaneId,
    ) -> Result<(), String> {
        let guard = self.peer_client.borrow();
        let Some(client) = guard.as_ref() else {
            return Err(String::from("no outbound Haven peer client active"));
        };
        client.generate_totp_code(peer_id, totp_secret_id, target_pane)
    }

    pub fn session_tree(&self) -> session::SessionTree {
        let ws = self.workspace.borrow();
        let inv = self.session_cache.borrow();
        let cfg = self.config.borrow();
        let peers = self.list_peers();
        let limb_invs = self.limb_inventories.borrow();
        session::build_session_tree(
            &ws,
            &inv,
            &cfg.session_hosts,
            cfg.root_session.as_deref(),
            &peers,
            &limb_invs,
        )
    }

    fn session_tree_response(&self) -> IpcResponse {
        let mut resp = IpcResponse::empty_ok();
        if let IpcResponse::Ok { session_tree, .. } = &mut resp {
            *session_tree = Some(self.session_tree());
        }
        resp
    }

    pub fn refresh_haven_limb_inventories(&self) -> bool {
        if let Some(client) = self.peer_client.borrow().as_ref() {
            client.refresh_limb_inventories();
            true
        } else {
            false
        }
    }

    pub fn focus_haven_terminal_session(
        &self,
        peer_id: &str,
        session_id: &str,
    ) -> Result<(), String> {
        let guard = self.peer_client.borrow();
        let Some(client) = guard.as_ref() else {
            return Err(String::from("no outbound Haven peer client active"));
        };
        client.focus_remote_session(peer_id.to_owned(), session_id.to_owned())
    }

    /// Opens up to `9` multiplexer sessions tiled into a balanced multi-pane
    /// overview window in one operation (`Q20`).
    pub fn open_session_grid(
        self: &Rc<Self>,
        sessions: Vec<String>,
        kind: Option<MuxKind>,
        host: Option<&str>,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        const MAX_GRID_PANES: usize = 9;
        if let Some(h) = host {
            mux::validate_host(h).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        }
        let target_host: Option<String> = host.map(|h| mux::sanitize_text(h, 64));
        let mut targets: Vec<(MuxKind, String, Option<String>)> = Vec::new();
        if !sessions.is_empty() {
            let resolved_kind = kind
                .or_else(|| self.config.borrow().preferred_mux)
                .unwrap_or(MuxKind::Tmux);
            for raw in sessions {
                let clean = mux::sanitize_text(raw.trim(), 64);
                if clean.is_empty() || targets.iter().any(|(_, n, _)| n == &clean) {
                    continue;
                }
                targets.push((resolved_kind, clean, target_host.clone()));
                if targets.len() >= MAX_GRID_PANES {
                    break;
                }
            }
        } else {
            let cache = self.session_cache.borrow();
            let mut candidates: Vec<&MuxSession> = cache
                .sessions
                .iter()
                .filter(|s| {
                    !s.exited
                        && s.host.as_deref() == target_host.as_deref()
                        && kind.is_none_or(|k| s.kind == k)
                })
                .collect();
            candidates.sort_by(|a, b| a.name.cmp(&b.name));
            let unattached: Vec<&MuxSession> = candidates
                .iter()
                .copied()
                .filter(|s| s.kind != MuxKind::Tmux || s.attached == 0)
                .collect();
            let chosen = if !unattached.is_empty() {
                unattached
            } else {
                candidates
            };
            for s in chosen.into_iter().take(MAX_GRID_PANES) {
                if !targets.iter().any(|(k, n, _)| *k == s.kind && n == &s.name) {
                    targets.push((s.kind, s.name.clone(), s.host.clone()));
                }
            }
        }

        if targets.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no multiplexer sessions available for grid; pass --sessions or start sessions first",
            ));
        }

        let opts = self.config.borrow().sessions.clone();
        let inherited_cwd = if target_host.is_some() {
            None
        } else {
            self.active_cwd(None, None)
        };
        let mut entries = Vec::with_capacity(targets.len());
        for (k, name, h) in targets {
            let command = match h.as_deref() {
                None => mux::attach_argv(k, &name, &opts),
                Some(remote) => mux::remote_attach_argv(remote, k, &name, &opts)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
            };
            let pid = self.workspace.borrow_mut().reserve_pane_id();
            let mut engine = self.spawn_engine(pid, inherited_cwd.clone(), Some(command))?;
            engine.set_title(name.clone());
            engine.set_mux_session(Some(mux::MuxSessionRef {
                kind: k,
                name,
                host: h,
            }));
            entries.push((pid, engine));
        }
        let (win_id, tab_id, first_pane) = self
            .workspace
            .borrow_mut()
            .insert_window_with_grid(entries, self.default_menubar.get())
            .expect("targets validated non-empty");
        self.ensure_ui_window(win_id);
        self.sync_ui_window(win_id);
        Ok((win_id, tab_id, first_pane))
    }

    pub fn detect_pane_mux_backing(
        &self,
        pid: PaneId,
    ) -> Result<Option<mux::MuxSessionRef>, String> {
        let ws = self.workspace.borrow();
        let Some(eng) = ws.pane(pid) else {
            return Err(format!("pane {pid} not found"));
        };
        if let Some(mref) = eng.mux_session() {
            return Ok(Some(mref.clone()));
        }
        if let Some((kind, name)) = eng.pty_raw_fd().and_then(mux::detect_foreground_mux_on_pty) {
            return Ok(Some(mux::MuxSessionRef {
                kind,
                name,
                host: None,
            }));
        }
        if let Some((kind, name)) = mux::detect_mux_from_text(eng.title()) {
            return Ok(Some(mux::MuxSessionRef {
                kind,
                name,
                host: None,
            }));
        }
        Ok(None)
    }

    fn resolve_target_outbound_peer_id(&self, requested: Option<&str>) -> Result<String, String> {
        let store = self.peer_store.borrow();
        let statuses = self.outbound_statuses.borrow();
        if let Some(req) = requested.map(str::trim).filter(|s| !s.is_empty()) {
            let Some(peer) = store
                .outbound_peers
                .iter()
                .find(|p| p.id == req || p.label == req)
            else {
                return Err(format!("no paired outbound Haven peer matching {req:?}"));
            };
            if matches!(
                statuses.get(&peer.id),
                Some(OutboundLimbStatus::Offline { .. })
            ) {
                return Err(format!("Haven peer '{}' is offline", peer.label));
            }
            return Ok(peer.id.clone());
        }
        if let Some(connected_id) = store.outbound_peers.iter().find_map(|p| {
            matches!(
                statuses.get(&p.id),
                Some(OutboundLimbStatus::Connected { .. })
            )
            .then(|| p.id.clone())
        }) {
            return Ok(connected_id);
        }
        if let Some(first) = store.outbound_peers.first() {
            if matches!(
                statuses.get(&first.id),
                Some(OutboundLimbStatus::Offline { .. })
            ) {
                return Err(format!("Haven peer '{}' is offline", first.label));
            }
            return Ok(first.id.clone());
        }
        Err(String::from("no paired outbound Haven peer"))
    }

    pub fn begin_handoff_to_peer(
        &self,
        pane: Option<PaneId>,
        peer: Option<String>,
        profile_id: Option<String>,
        reply: Option<UnixStream>,
    ) -> Result<(), String> {
        let pid = pane
            .or_else(|| self.workspace.borrow().active_pane_id())
            .ok_or_else(|| String::from("no active pane"))?;
        let Some(mux_ref) = self.detect_pane_mux_backing(pid)? else {
            let notice = String::from(
                "Bare non-mux shell cannot be handed off live; attach a tmux or zellij session (Ctrl+Shift+S) first.",
            );
            if let Some(eng) = self.workspace.borrow_mut().pane_mut(pid) {
                eng.set_agent_notice(Some(notice));
            }
            self.refresh_all_window_chromes();
            return Err(format!(
                "pane {pid} is a bare non-mux shell; attach it to a tmux or zellij session (open-session / new-session) for live handoff to Haven"
            ));
        };

        let peer_id = self.resolve_target_outbound_peer_id(peer.as_deref())?;
        if self.peer_client.borrow().is_none() {
            let initial = self.peer_store.borrow().outbound_peers.clone();
            let cfg = self.peer_poll_config.borrow().clone();
            *self.peer_client.borrow_mut() = Some(HavenPeerClientHandle::spawn(initial, cfg));
            self.force_push_shade_poll_state();
        }
        let req_id = self.next_pair_req_id.get();
        self.next_pair_req_id.set(req_id + 1);
        if let Some(stream) = reply {
            self.pending_handoff_replies
                .borrow_mut()
                .insert(req_id, stream);
        }
        let default_profile = self.config.borrow().default_haven_ssh_profile.clone();
        let send_res = self
            .peer_client
            .borrow()
            .as_ref()
            .expect("peer client spawned")
            .handoff_to_peer(
                req_id,
                peer_id,
                mux_ref.name,
                mux_ref.host,
                profile_id,
                default_profile,
                mux::local_hostname(),
            );
        if let Err(err) = send_res {
            self.pending_handoff_replies.borrow_mut().remove(&req_id);
            return Err(err);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin_present_to_peer(
        &self,
        peer: Option<String>,
        url: Option<String>,
        path: Option<PathBuf>,
        text: Option<String>,
        clipboard: bool,
        profile_id: Option<String>,
        caption: Option<String>,
        reply: Option<UnixStream>,
    ) -> Result<(), String> {
        let resolved_text = if text.is_some() {
            text
        } else if clipboard {
            let sel = self.workspace.borrow().active_pane_id().and_then(|pid| {
                self.workspace
                    .borrow()
                    .pane(pid)
                    .and_then(|e| e.selection_text())
            });
            match sel {
                Some(s) if !s.is_empty() => Some(s),
                _ => {
                    return Err(String::from(
                        "no active terminal selection to copy; select text in a pane or pass --text",
                    ));
                }
            }
        } else {
            None
        };
        let plan = classify_present_target(
            url.as_deref(),
            path.as_deref(),
            resolved_text.as_deref(),
            profile_id.as_deref(),
            caption.as_deref(),
        )?;
        let peer_id = self.resolve_target_outbound_peer_id(peer.as_deref())?;
        if self.peer_client.borrow().is_none() {
            let initial = self.peer_store.borrow().outbound_peers.clone();
            let cfg = self.peer_poll_config.borrow().clone();
            *self.peer_client.borrow_mut() = Some(HavenPeerClientHandle::spawn(initial, cfg));
            self.force_push_shade_poll_state();
        }
        let req_id = self.next_pair_req_id.get();
        self.next_pair_req_id.set(req_id + 1);
        if let Some(stream) = reply {
            self.pending_present_replies
                .borrow_mut()
                .insert(req_id, stream);
        }
        let default_profile = self.config.borrow().default_haven_ssh_profile.clone();
        let send_res = self
            .peer_client
            .borrow()
            .as_ref()
            .expect("peer client spawned")
            .present_to_peer(
                req_id,
                peer_id,
                plan,
                default_profile,
                mux::local_hostname(),
            );
        if let Err(err) = send_res {
            self.pending_present_replies.borrow_mut().remove(&req_id);
            return Err(err);
        }
        Ok(())
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
                self.dispatch_peer.set(item.peer);
                self.dispatch_carrier.set(item.carrier);
                *self.dispatch_client_label.borrow_mut() = item.client.clone();
                if let Some(resp) = self.execute_ipc_request(item.request, &item.stream) {
                    let _ = write_response(&mut item.stream, &resp);
                }
            }
            ConsentResolution::AllowSession => {
                // Tier-4 Irreversible and policy-management verbs never create
                // or ride on a standing session grant.
                let can_grant_session = item.request.consent_tier()
                    == crate::action::ConsentTier::Act
                    && !matches!(
                        item.request,
                        IpcRequest::InstallPolicy { .. } | IpcRequest::RevokePolicy { .. }
                    );
                let decision = if can_grant_session {
                    // Bind to the verified peer, not to the label it sent.
                    self.grant_session(item.peer, &item.client);
                    AuditDecision::ApprovedSession
                } else {
                    AuditDecision::ApprovedOnce
                };
                self.record_audit(
                    &item.client,
                    &item.request,
                    decision,
                    item.carrier,
                    item.peer,
                );
                self.dispatch_peer.set(item.peer);
                self.dispatch_carrier.set(item.carrier);
                *self.dispatch_client_label.borrow_mut() = item.client.clone();
                if let Some(resp) = self.execute_ipc_request(item.request, &item.stream) {
                    let _ = write_response(&mut item.stream, &resp);
                }
            }
        }
        self.refresh_all_window_chromes();
    }

    /// `command` overrides the controller-wide shell for this one pane, which is
    /// how a multiplexer attach runs `tmux`/`ssh` instead of a login shell.
    fn spawn_engine(
        &self,
        pane_id: PaneId,
        cwd: Option<PathBuf>,
        command: Option<(String, Vec<String>)>,
    ) -> io::Result<PaneEngine> {
        let (cols, rows, scrollback, bg, fg) = {
            let cfg = self.config.borrow();
            (
                cfg.initial_cols,
                cfg.initial_rows,
                cfg.scrollback_lines,
                cfg.background,
                cfg.foreground,
            )
        };
        let size = GridSize::new(cols, rows, 9, 18);
        let shell = command.or_else(|| self.shell_override.borrow().clone());
        let mut engine = PaneEngine::spawn_pty(size, cwd, shell, pane_id.0, scrollback)?;
        // OSC 10/11 queries must answer with the colors actually rendered.
        engine.set_reported_colors(bg, fg);
        Ok(engine)
    }

    /// Attaches a pane to an existing multiplexer session, locally or over SSH.
    /// The multiplexer owns process survival; closing the pane kills only the
    /// client, never the server.
    pub fn open_session(
        self: &Rc<Self>,
        kind: MuxKind,
        name: &str,
        host: Option<&str>,
        mode: mux::SessionMode,
        window: Option<WindowId>,
        pane: Option<PaneId>,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        if name.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session name must not be empty",
            ));
        }
        self.attach_or_create(
            SessionRequest { kind, name, host },
            mode,
            window,
            pane,
            false,
        )
    }

    /// Creates a NEW session and opens a pane on it. Unlike `open_session` this
    /// must fail on a name collision rather than attaching to whatever already
    /// has that name, so it uses the create argv (no tmux `-A`).
    pub fn new_session(
        self: &Rc<Self>,
        kind: Option<MuxKind>,
        name: Option<String>,
        mode: mux::SessionMode,
        window: Option<WindowId>,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        let Some(kind) = kind else {
            // No multiplexer: a plain shell, which is what `preferred = "none"`
            // and the dialog's "none" choice mean.
            let cwd = self.active_cwd(window, None);
            return match mode {
                mux::SessionMode::Window => self.create_window(cwd),
                mux::SessionMode::Tab => self.create_tab(window, cwd),
                mux::SessionMode::SplitRight => self.split_pane(None, SplitAxis::Horizontal, cwd),
                mux::SessionMode::SplitDown => self.split_pane(None, SplitAxis::Vertical, cwd),
            };
        };
        let name = match name {
            Some(n) if !n.trim().is_empty() => n,
            _ => self.suggested_session_name(),
        };
        self.attach_or_create(
            SessionRequest {
                kind,
                name: &name,
                host: None,
            },
            mode,
            window,
            None,
            true,
        )
    }

    /// Renames a multiplexer session using its own command, then updates every
    /// pane attached to it. Returns once the command is launched; the rename is
    /// applied when it exits, because a remote rename can take seconds.
    pub fn rename_session(
        &self,
        kind: MuxKind,
        old: &str,
        new: &str,
        host: Option<&str>,
    ) -> io::Result<()> {
        self.rename_session_with_reply(kind, old, new, host, None)
            .map(|_| ())
    }

    fn rename_session_with_reply(
        &self,
        kind: MuxKind,
        old: &str,
        new: &str,
        host: Option<&str>,
        reply: Option<UnixStream>,
    ) -> io::Result<Option<IpcResponse>> {
        if new.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the new session name must not be empty",
            ));
        }
        if old == new {
            return Ok(Some(IpcResponse::empty_ok()));
        }
        let opts = self.config.borrow().sessions.clone();
        let (program, argv) = mux::rename_command(kind, old, new, host, &opts)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        self.launch_mux_command(
            &program,
            &argv,
            MuxEffect::Rename {
                kind,
                host: host.map(str::to_owned),
                old: old.to_owned(),
                new: new.to_owned(),
            },
            reply,
        )?;
        Ok(None)
    }

    /// Ends a multiplexer session. DESTRUCTIVE: everything running inside it
    /// stops, unlike closing a tab, which only ends this client. The UI gates
    /// this behind an explicit confirmation naming the session.
    pub fn end_session(&self, kind: MuxKind, name: &str, host: Option<&str>) -> io::Result<()> {
        self.end_session_with_reply(kind, name, host, None)
            .map(|_| ())
    }

    fn end_session_with_reply(
        &self,
        kind: MuxKind,
        name: &str,
        host: Option<&str>,
        reply: Option<UnixStream>,
    ) -> io::Result<Option<IpcResponse>> {
        let opts = self.config.borrow().sessions.clone();
        let (program, argv) = mux::end_command(kind, name, host, &opts)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        self.launch_mux_command(
            &program,
            &argv,
            MuxEffect::End {
                kind,
                name: name.to_owned(),
            },
            reply,
        )?;
        Ok(None)
    }

    fn launch_mux_command(
        &self,
        program: &str,
        argv: &[String],
        effect: MuxEffect,
        reply: Option<UnixStream>,
    ) -> io::Result<()> {
        let child = Command::new(program)
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("could not run {program}: {e}")))?;
        if let Some(out) = child.stdout.as_ref() {
            set_nonblocking(out.as_raw_fd());
        }
        if let Some(err) = child.stderr.as_ref() {
            set_nonblocking(err.as_raw_fd());
        }
        let timeout = Duration::from_millis(self.config.borrow().discovery_timeout_ms);
        self.mux_commands.borrow_mut().push(PendingMuxCommand {
            child,
            out: Vec::new(),
            err: Vec::new(),
            deadline: Instant::now() + timeout,
            killed: false,
            effect,
            reply,
        });
        Ok(())
    }

    /// Polls one-shot commands. Same nonblocking drain and kill-then-reap rules
    /// as discovery: `Child::drop` does neither.
    fn advance_mux_commands(self: &Rc<Self>) {
        if self.mux_commands.borrow().is_empty() {
            return;
        }
        let mut budget = DISCOVERY_BYTES_PER_TICK;
        let mut done: Vec<(MuxEffect, Option<UnixStream>, bool, String)> = Vec::new();
        {
            let mut jobs = self.mux_commands.borrow_mut();
            let mut i = 0;
            while i < jobs.len() {
                let job = &mut jobs[i];
                if let Some(out) = job.child.stdout.as_mut() {
                    drain_bounded(out, &mut job.out, &mut budget);
                }
                if let Some(err) = job.child.stderr.as_mut() {
                    drain_bounded(err, &mut job.err, &mut budget);
                }
                if !job.killed && Instant::now() >= job.deadline {
                    let _ = job.child.kill();
                    job.killed = true;
                }
                match job.child.try_wait() {
                    Ok(Some(status)) => {
                        let ok = !job.killed && status.success();
                        let detail = if job.killed {
                            String::from("timed out")
                        } else {
                            let stderr = String::from_utf8_lossy(&job.err);
                            let stdout = String::from_utf8_lossy(&job.out);
                            let first = stderr
                                .lines()
                                .find(|l| !l.trim().is_empty())
                                .or_else(|| stdout.lines().find(|l| !l.trim().is_empty()))
                                .unwrap_or("")
                                .trim();
                            let clean = mux::sanitize_text(first, 160);
                            if clean.is_empty() {
                                format!("exited with {:?}", status.code())
                            } else {
                                clean
                            }
                        };
                        let finished = jobs.remove(i);
                        done.push((finished.effect, finished.reply, ok, detail));
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let finished = jobs.remove(i);
                        done.push((finished.effect, finished.reply, false, e.to_string()));
                        continue;
                    }
                }
                i += 1;
            }
        }
        let mut changed = false;
        for (effect, reply, ok, detail) in done {
            match effect {
                MuxEffect::End { kind, name } => {
                    if ok {
                        changed = true;
                        if let Some(mut stream) = reply {
                            let _ = write_response(&mut stream, &IpcResponse::empty_ok());
                        }
                    } else {
                        let msg = format!(
                            "ending {} session {:?} failed: {detail}",
                            kind.label(),
                            mux::sanitize_text(&name, 64)
                        );
                        match reply {
                            Some(mut stream) => {
                                let _ = write_response(&mut stream, &IpcResponse::err(msg));
                            }
                            None => {
                                eprintln!("warning: {msg}");
                                self.surface_window_notice(None, msg);
                            }
                        }
                    }
                }
                MuxEffect::Rename {
                    kind,
                    host,
                    old,
                    new,
                } => {
                    if ok {
                        self.apply_rename(kind, host.as_deref(), &old, &new);
                        changed = true;
                        if let Some(mut stream) = reply {
                            let _ = write_response(&mut stream, &IpcResponse::empty_ok());
                        }
                    } else {
                        let msg = format!(
                            "rename {:?} to {:?} failed: {detail}",
                            mux::sanitize_text(&old, 64),
                            mux::sanitize_text(&new, 64)
                        );
                        match reply {
                            Some(mut stream) => {
                                let _ = write_response(&mut stream, &IpcResponse::err(msg));
                            }
                            None => {
                                if !new.is_empty() {
                                    eprintln!("warning: {msg}");
                                    let target_pane = {
                                        let ws = self.workspace.borrow();
                                        ws.pane_ids().into_iter().find(|&pid| {
                                            ws.pane(pid).and_then(|e| e.mux_session()).is_some_and(
                                                |s| {
                                                    s.kind == kind
                                                        && s.host.as_deref() == host.as_deref()
                                                        && s.name == old
                                                },
                                            )
                                        })
                                    };
                                    if let Some(pid) = target_pane {
                                        if let Some(eng) = self.workspace.borrow_mut().pane_mut(pid)
                                        {
                                            eng.set_agent_notice(Some(msg));
                                        }
                                        changed = true;
                                    } else {
                                        self.surface_window_notice(None, msg);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if changed {
            self.refresh_all_window_chromes();
            let targets = self.discovery_targets_all();
            self.begin_discovery(targets, true, None);
        }
    }

    /// Updates every pane attached to the renamed session, so labels, window
    /// titles, and the saved bundle all follow the multiplexer's new name.
    fn apply_rename(&self, kind: MuxKind, host: Option<&str>, old: &str, new: &str) {
        let mut ws = self.workspace.borrow_mut();
        for pid in ws.pane_ids() {
            if let Some(eng) = ws.pane_mut(pid) {
                let matches = eng
                    .mux_session()
                    .is_some_and(|s| s.kind == kind && s.host.as_deref() == host && s.name == old);
                if matches {
                    eng.set_mux_session(Some(mux::MuxSessionRef {
                        kind,
                        name: mux::sanitize_text(new, 64),
                        host: host.map(|h| mux::sanitize_text(h, 64)),
                    }));
                }
            }
        }
    }

    /// Smallest free `mutiny-N` across the discovery cache and live workspace panes.
    pub fn suggested_session_name(&self) -> String {
        let mut taken: Vec<String> = self
            .session_cache
            .borrow()
            .sessions
            .iter()
            .map(|s| s.name.clone())
            .collect();
        {
            let ws = self.workspace.borrow();
            for pid in ws.pane_ids() {
                if let Some(m) = ws.pane(pid).and_then(|e| e.mux_session())
                    && !taken.iter().any(|t| t == &m.name)
                {
                    taken.push(m.name.clone());
                }
            }
        }
        mux::free_session_name(&taken)
    }

    /// Checks whether a local multiplexer session named `name` already exists
    /// (live, not exited), consulting `session_cache` first and falling back to a
    /// bounded (<250 ms) local discovery probe so `New Session` in the GUI can
    /// offer to attach instead of failing silently on a duplicate name.
    pub fn local_session_exists(&self, kind: MuxKind, name: &str) -> bool {
        let wanted = name.trim();
        if wanted.is_empty() {
            return false;
        }
        if self
            .session_cache
            .borrow()
            .sessions
            .iter()
            .any(|s| s.host.is_none() && s.kind == kind && s.name == wanted && !s.exited)
        {
            return true;
        }
        let opts = self.config.borrow().sessions.clone();
        let program = mux::local_program(kind, &opts);
        let argv = mux::discovery_argv(kind, &opts);
        let Ok(mut child) = Command::new(&program)
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        if let Some(out) = child.stdout.as_ref() {
            set_nonblocking(out.as_raw_fd());
        }
        let mut buf = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            let mut budget = DISCOVERY_BYTES_PER_TICK;
            if let Some(out) = child.stdout.as_mut() {
                drain_bounded(out, &mut buf, &mut budget);
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut budget = DISCOVERY_BYTES_PER_TICK;
                    if let Some(out) = child.stdout.as_mut() {
                        drain_bounded(out, &mut buf, &mut budget);
                    }
                    if !status.success() {
                        return false;
                    }
                    let text = String::from_utf8_lossy(&buf);
                    let sessions = match kind {
                        MuxKind::Tmux => mux::parse_tmux(&text, None),
                        MuxKind::Zellij => mux::parse_zellij(&text, None),
                    };
                    return sessions.iter().any(|s| s.name == wanted && !s.exited);
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    }

    /// Surfaces an operator-facing error or diagnostic notice on the active pane
    /// of `preferred_win` (or the workspace active pane) and refreshes banners.
    pub fn surface_window_notice(&self, preferred_win: Option<WindowId>, message: String) {
        let target_pane = {
            let ws = self.workspace.borrow();
            preferred_win
                .and_then(|wid| ws.window(wid).and_then(|w| w.active_pane()))
                .or_else(|| ws.active_pane_id())
        };
        if let Some(pid) = target_pane {
            if let Some(eng) = self.workspace.borrow_mut().pane_mut(pid) {
                eng.set_agent_notice(Some(message));
            }
            self.refresh_all_window_chromes();
        }
    }

    fn attach_or_create(
        self: &Rc<Self>,
        request: SessionRequest<'_>,
        mode: mux::SessionMode,
        window: Option<WindowId>,
        pane: Option<PaneId>,
        create: bool,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        let SessionRequest { kind, name, host } = request;
        if name.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session name must not be empty",
            ));
        }
        let opts = self.config.borrow().sessions.clone();
        let command = match (host, create) {
            (None, false) => mux::attach_argv(kind, name, &opts),
            (None, true) => mux::create_argv(kind, name, &opts),
            (Some(host), false) => mux::remote_attach_argv(host, kind, name, &opts)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
            (Some(host), true) => mux::remote_create_argv(host, kind, name, &opts)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
        };
        // A remote session's path is a path on that host, so it must never
        // become a local cwd. alacritty ignores an invalid chdir silently.
        let cwd = if host.is_some() {
            None
        } else {
            self.active_cwd(window, pane)
        };
        let ids = match mode {
            mux::SessionMode::Window => self.create_window_with_command(cwd, Some(command))?,
            mux::SessionMode::Tab => self.create_tab_with_command(window, cwd, Some(command))?,
            mux::SessionMode::SplitRight => {
                self.split_pane_with_command(pane, SplitAxis::Horizontal, cwd, Some(command))?
            }
            mux::SessionMode::SplitDown => {
                self.split_pane_with_command(pane, SplitAxis::Vertical, cwd, Some(command))?
            }
        };
        if let Some(engine) = self.workspace.borrow_mut().pane_mut(ids.2) {
            engine.set_mux_session(Some(mux::MuxSessionRef {
                kind,
                name: mux::sanitize_text(name, 64),
                host: host.map(|h| mux::sanitize_text(h, 64)),
            }));
        }
        self.sync_ui_window(ids.0);
        Ok(ids)
    }

    /// Local plus every configured remote host. For the GUI picker, where the
    /// human is the consent. The IPC verb never uses this: `host: None` there
    /// means local only, so an Observe call cannot spend the user's credentials.
    pub fn discovery_targets_all(&self) -> Vec<Option<String>> {
        let mut targets = vec![None];
        let hosts = self.config.borrow().session_hosts.clone();
        let hosts = if hosts.is_empty() {
            std::fs::read_to_string(ssh_config_path())
                .map(|text| mux::ssh_config_hosts(&text))
                .unwrap_or_default()
        } else {
            hosts
        };
        for host in hosts.into_iter().take(mux::MAX_HOSTS_PER_REQUEST) {
            targets.push(Some(host));
        }
        targets
    }

    /// Expires the agent freshness window immediately. The window is 15 s in
    /// production, which is too long for a test to wait out.
    pub fn clear_recent_agent_for_test(&self) {
        *self.recent_agent.borrow_mut() = None;
    }

    /// Re-runs discovery on a modest cadence so the Sessions submenu reflects
    /// sessions that appeared or died since startup. This polls an external
    /// listing command; it owns no sessions and starts no daemon, so the
    /// crash-semantics contract is untouched.
    fn refresh_sessions_periodically(&self) {
        if self.gtk_app.borrow().is_none() || self.ui_windows.borrow().is_empty() {
            return;
        }
        let interval = self.config.borrow().session_refresh_interval_s;
        if interval == 0 || self.discovery_in_flight() {
            return;
        }
        let due = match self.last_discovery_started.get() {
            None => true,
            Some(at) => at.elapsed() >= Duration::from_secs(u64::from(interval)),
        };
        if due {
            let targets = self.discovery_targets_all();
            self.begin_discovery(targets, true, None);
        }
    }

    /// Queues a policy proposal. Grants nothing: the operator installs it from
    /// the Agent activity dialog. Bound to the proposing peer, so a policy can
    /// never be installed for a different one.
    pub fn propose_policy(
        &self,
        peer: PeerIdentity,
        description: String,
        verbs: Vec<String>,
        arg_constraints: std::collections::BTreeMap<String, String>,
        max_calls_per_minute: u32,
        expires_in_minutes: u64,
    ) -> Result<u64, String> {
        if self.policies.borrow().len() >= crate::policy::MAX_POLICIES {
            return Err(format!(
                "at most {} policies; revoke one first",
                crate::policy::MAX_POLICIES
            ));
        }
        let id = self.next_policy_id.get();
        let lifetime = expires_in_minutes.clamp(1, crate::policy::MAX_LIFETIME_MINUTES);
        let proposal = crate::policy::StandingPolicy {
            id,
            peer,
            description,
            verbs,
            arg_constraints,
            max_calls_per_minute,
            expires_at_ms: now_ms() + lifetime * 60_000,
        }
        .sanitised()?;
        self.next_policy_id.set(id + 1);
        self.pending_policies.borrow_mut().push(proposal);
        self.refresh_all_window_chromes();
        Ok(id)
    }

    /// Operator-side install. Moves a proposal into force.
    pub fn install_policy(&self, id: u64) -> Result<(), String> {
        let mut pending = self.pending_policies.borrow_mut();
        let Some(at) = pending.iter().position(|p| p.id == id) else {
            return Err(format!("no pending policy {id}"));
        };
        let policy = pending.remove(at);
        drop(pending);
        self.policies.borrow_mut().push(policy);
        self.refresh_all_window_chromes();
        Ok(())
    }

    pub fn revoke_policy(&self, id: u64) -> bool {
        let mut installed = self.policies.borrow_mut();
        let before = installed.len();
        installed.retain(|p| p.id != id);
        let removed = installed.len() != before;
        drop(installed);
        self.pending_policies.borrow_mut().retain(|p| p.id != id);
        self.policy_uses.borrow_mut().remove(&id);
        if removed {
            self.refresh_all_window_chromes();
        }
        removed
    }

    pub fn installed_policies(&self) -> Vec<crate::policy::StandingPolicy> {
        self.policies.borrow().clone()
    }

    pub fn pending_policies(&self) -> Vec<crate::policy::StandingPolicy> {
        self.pending_policies.borrow().clone()
    }

    /// True while an external agent is driving the terminal. Derived from the
    /// same signal as the in-window agent badge, so the icon and the badge can
    /// never disagree: a non-Operator carrier call within the last 15 seconds.
    /// That window is also the debounce, so a brief act cannot flicker.
    pub fn agent_active(&self) -> bool {
        self.active_agent_badge().is_some()
    }

    /// Pushes the agent-active icon state to the default and every window, but
    /// only on a transition.
    fn refresh_agent_icon(&self) {
        // poll_once also runs headlessly, where GTK is not initialised and any
        // GTK call panics. No application means no windows to iconify.
        if self.gtk_app.borrow().is_none() {
            return;
        }
        let active = self.agent_active();
        if self.agent_icon_active.get() == active {
            return;
        }
        self.agent_icon_active.set(active);
        let name = if active {
            crate::desktop::AGENT_ICON_NAME
        } else {
            crate::desktop::APP_ID
        };
        gtk4::Window::set_default_icon_name(name);
        let wins: Vec<MutinyWindow> = self.ui_windows.borrow().values().cloned().collect();
        for w in wins {
            w.set_agent_active(active);
        }
    }

    /// Registers an application-level Sessions submenu (not tied to a single
    /// window) to be filled from discovery results.
    pub fn register_sessions_menu(&self, menu: gtk4::gio::Menu) {
        crate::action::populate_sessions_menu(&menu, &self.session_inventory());
        self.sessions_menus.borrow_mut().push((None, menu));
    }

    /// Registers a per-window Sessions submenu so closing that window prunes it.
    pub fn register_window_sessions_menu(&self, win_id: WindowId, menu: gtk4::gio::Menu) {
        crate::action::populate_sessions_menu(&menu, &self.session_inventory());
        self.sessions_menus.borrow_mut().push((Some(win_id), menu));
    }

    pub fn sessions_menu_count(&self) -> usize {
        self.sessions_menus.borrow().len()
    }

    pub fn session_inventory(&self) -> MuxInventory {
        self.session_cache.borrow().clone()
    }

    pub fn session_cache_gen(&self) -> u64 {
        self.cache_gen.get()
    }

    pub fn discovery_in_flight(&self) -> bool {
        !self.discovery_jobs.borrow().is_empty() || !self.discovery_queue.borrow().is_empty()
    }

    /// Starts a discovery pass. Returns `Some` when the answer is already known
    /// (cached, or a pass is running and this caller gets the current snapshot),
    /// and `None` when the stream is parked until the pass completes.
    pub fn begin_discovery(
        &self,
        targets: Vec<Option<String>>,
        refresh: bool,
        stream: Option<UnixStream>,
    ) -> Option<IpcResponse> {
        if !refresh && !self.discovery_in_flight() && !self.session_cache.borrow().hosts.is_empty()
        {
            return Some(self.inventory_response());
        }
        // A second requester while a pass runs gets the snapshot rather than a
        // second parked stream: one reply slot, bounded.
        if self.discovery_in_flight() {
            if let Some(mut stream) = stream {
                let _ = write_response(&mut stream, &self.inventory_response());
            }
            return None;
        }

        let timeout = Duration::from_millis(self.config.borrow().discovery_timeout_ms);
        {
            let mut cache = self.session_cache.borrow_mut();
            // Keep the previous sessions until each target's result replaces
            // its slice. Clearing here would make the Sessions submenu flash
            // "No sessions found" at the start of every refresh.
            cache.hosts.clear();
            cache.partial = false;
            cache.scanned_at_ms = now_ms();
            for target in &targets {
                for kind in [MuxKind::Tmux, MuxKind::Zellij] {
                    cache.hosts.push(MuxHostStatus {
                        host: target.clone(),
                        kind,
                        state: HostState::Scanning,
                        error: None,
                        elapsed_ms: 0,
                    });
                }
            }
        }
        self.bump_cache_gen();
        {
            let mut queue = self.discovery_queue.borrow_mut();
            queue.clear();
            for target in targets {
                for kind in [MuxKind::Tmux, MuxKind::Zellij] {
                    queue.push_back((target.clone(), kind));
                }
            }
        }
        *self.pending_session_reply.borrow_mut() = stream;
        self.last_discovery_started.set(Some(Instant::now()));
        self.discovery_deadline.set(Some(Instant::now() + timeout));
        self.start_queued_discoveries();
        // Every program may have failed to spawn synchronously (not installed),
        // in which case the pass is already over and no tick will complete it.
        self.complete_discovery_pass();
        None
    }

    /// Writes the parked reply once no job or queued target remains. Called both
    /// from the tick and right after starting a pass, because a pass whose every
    /// spawn failed finishes before any tick runs.
    fn complete_discovery_pass(&self) {
        if !self.discovery_jobs.borrow().is_empty() || !self.discovery_queue.borrow().is_empty() {
            return;
        }
        self.discovery_deadline.set(None);
        if let Some(mut stream) = self.pending_session_reply.borrow_mut().take() {
            let _ = write_response(&mut stream, &self.inventory_response());
        }
        self.maybe_finish_tree_reply();
    }

    fn maybe_finish_tree_reply(&self) {
        if self.discovery_in_flight() || self.tree_awaiting_limbs.get() {
            return;
        }
        if let Some(mut stream) = self.pending_tree_reply.borrow_mut().take() {
            let _ = write_response(&mut stream, &self.session_tree_response());
        }
    }

    fn inventory_response(&self) -> IpcResponse {
        let mut inventory = self.session_inventory();
        if self.unattached_filter.get() {
            // tmux reports a client count; zellij does not, so its sessions are
            // left alone rather than guessed at.
            inventory
                .sessions
                .retain(|s| s.kind != MuxKind::Tmux || s.attached == 0);
        }
        let mut resp = IpcResponse::empty_ok();
        if let IpcResponse::Ok { sessions, .. } = &mut resp {
            *sessions = Some(inventory);
        }
        resp
    }

    fn bump_cache_gen(&self) {
        self.cache_gen.set(self.cache_gen.get().wrapping_add(1));
    }

    fn start_queued_discoveries(&self) {
        loop {
            if self.discovery_jobs.borrow().len() >= MAX_CONCURRENT_DISCOVERIES {
                break;
            }
            let next = self.discovery_queue.borrow_mut().pop_front();
            let Some((host, kind)) = next else { break };
            match self.spawn_discovery(host.clone(), kind) {
                Ok(job) => self.discovery_jobs.borrow_mut().push(job),
                Err(err) => self.record_discovery_result(host, kind, Err(err), 0),
            }
        }
    }

    fn spawn_discovery(
        &self,
        host: Option<String>,
        kind: MuxKind,
    ) -> Result<PendingDiscovery, String> {
        let opts = self.config.borrow().sessions.clone();
        let (program, argv) = match &host {
            None => (
                mux::local_program(kind, &opts),
                mux::discovery_argv(kind, &opts),
            ),
            Some(host) => mux::remote_discovery_argv(host, kind, &opts)?,
        };
        let child = Command::new(&program)
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    format!("{}: not installed ({program})", kind.label())
                } else {
                    format!("{}: could not run {program}: {e}", kind.label())
                }
            })?;
        if let Some(out) = child.stdout.as_ref() {
            set_nonblocking(out.as_raw_fd());
        }
        if let Some(err) = child.stderr.as_ref() {
            set_nonblocking(err.as_raw_fd());
        }
        let started = Instant::now();
        let timeout = Duration::from_millis(self.config.borrow().discovery_timeout_ms);
        Ok(PendingDiscovery {
            host,
            kind,
            child,
            out: Vec::new(),
            err: Vec::new(),
            started,
            deadline: started + timeout,
            killed: false,
        })
    }

    /// Polls discovery children without blocking. Called from `poll_once`.
    fn advance_discoveries(self: &Rc<Self>) {
        if self.discovery_jobs.borrow().is_empty() && self.discovery_queue.borrow().is_empty() {
            self.complete_discovery_pass();
            return;
        }
        let mut budget = DISCOVERY_BYTES_PER_TICK;
        let mut finished: Vec<DiscoveryOutcome> = Vec::new();
        {
            let mut jobs = self.discovery_jobs.borrow_mut();
            let mut index = 0;
            while index < jobs.len() {
                let job = &mut jobs[index];
                if let Some(out) = job.child.stdout.as_mut() {
                    drain_bounded(out, &mut job.out, &mut budget);
                }
                if let Some(err) = job.child.stderr.as_mut() {
                    drain_bounded(err, &mut job.err, &mut budget);
                }
                if !job.killed && Instant::now() >= job.deadline {
                    let _ = job.child.kill();
                    job.killed = true;
                }
                match job.child.try_wait() {
                    Ok(Some(status)) => {
                        let elapsed = job.started.elapsed().as_millis() as u64;
                        let stdout = String::from_utf8_lossy(&job.out).into_owned();
                        let stderr = String::from_utf8_lossy(&job.err).into_owned();
                        let result = if job.killed {
                            Err(format!("{}: timed out", job.kind.label()))
                        } else {
                            parse_discovery(
                                job.kind,
                                &stdout,
                                &stderr,
                                status.code(),
                                job.host.as_deref(),
                            )
                        };
                        finished.push((job.host.clone(), job.kind, result, elapsed));
                        jobs.remove(index);
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let elapsed = job.started.elapsed().as_millis() as u64;
                        finished.push((
                            job.host.clone(),
                            job.kind,
                            Err(format!("{}: {e}", job.kind.label())),
                            elapsed,
                        ));
                        jobs.remove(index);
                        continue;
                    }
                }
                index += 1;
            }
        }
        for (host, kind, result, elapsed) in finished {
            self.record_discovery_result(host, kind, result, elapsed);
        }
        self.start_queued_discoveries();

        self.complete_discovery_pass();
        self.refresh_session_pickers();
    }

    fn record_discovery_result(
        &self,
        host: Option<String>,
        kind: MuxKind,
        result: Result<Vec<MuxSession>, String>,
        elapsed_ms: u64,
    ) {
        let show_exited = self.config.borrow().show_exited_sessions;
        {
            let mut cache = self.session_cache.borrow_mut();
            cache
                .sessions
                .retain(|s| !(s.host == host && s.kind == kind));
            cache.hosts.retain(|h| !(h.host == host && h.kind == kind));
            let (state, error) = match &result {
                Ok(sessions) => {
                    for session in sessions {
                        if session.exited && !show_exited {
                            continue;
                        }
                        cache.sessions.push(session.clone());
                    }
                    (HostState::Ok, None)
                }
                Err(err) => (HostState::Error, Some(err.clone())),
            };
            cache.hosts.push(MuxHostStatus {
                host,
                kind,
                state,
                error,
                elapsed_ms,
            });
            if cache.sessions.len() > mux::MAX_SESSIONS_PER_HOST * mux::MAX_HOSTS_PER_REQUEST {
                cache
                    .sessions
                    .truncate(mux::MAX_SESSIONS_PER_HOST * mux::MAX_HOSTS_PER_REQUEST);
            }
        }
        self.bump_cache_gen();
    }

    /// Kills and reaps any outstanding discovery or mux command child.
    /// `Child::drop` does neither, so leaving them would leak zombies.
    fn shutdown_discoveries(&self) {
        self.discovery_queue.borrow_mut().clear();
        let jobs = std::mem::take(&mut *self.discovery_jobs.borrow_mut());
        for mut job in jobs {
            let _ = job.child.kill();
            let _ = job.child.wait();
        }
        let mux_jobs = std::mem::take(&mut *self.mux_commands.borrow_mut());
        for mut job in mux_jobs {
            let _ = job.child.kill();
            let _ = job.child.wait();
            if let Some(mut stream) = job.reply {
                let _ = write_response(&mut stream, &IpcResponse::err("server shutting down"));
            }
        }
        self.pending_session_reply.borrow_mut().take();
        if let Some(mut stream) = self.pending_tree_reply.borrow_mut().take() {
            let _ = write_response(&mut stream, &IpcResponse::err("server shutting down"));
        }
        self.tree_awaiting_limbs.set(false);
        self.discovery_deadline.set(None);
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
        self.create_window_with_command(cwd, None)
    }

    /// New window whose pane runs `command` instead of a login shell.
    pub fn create_window_with_command(
        self: &Rc<Self>,
        cwd: Option<PathBuf>,
        command: Option<(String, Vec<String>)>,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        let inherited_cwd = cwd.or_else(|| self.active_cwd(None, None));
        let pane_id = self.workspace.borrow_mut().reserve_pane_id();
        let engine = self.spawn_engine(pane_id, inherited_cwd, command)?;
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
        self.create_tab_with_command(target_win, cwd, None)
    }

    /// New tab whose pane runs `command` instead of a login shell.
    pub fn create_tab_with_command(
        self: &Rc<Self>,
        target_win: Option<WindowId>,
        cwd: Option<PathBuf>,
        command: Option<(String, Vec<String>)>,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        let win_id = target_win
            .or_else(|| self.workspace.borrow().active_window_id())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no active window"))?;
        let inherited_cwd = cwd.or_else(|| self.active_cwd(Some(win_id), None));
        let pane_id = self.workspace.borrow_mut().reserve_pane_id();
        let engine = self.spawn_engine(pane_id, inherited_cwd, command)?;
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
        self.split_pane_with_command(target_pane, axis, cwd, None)
    }

    /// New split whose pane runs `command` instead of a login shell.
    pub fn split_pane_with_command(
        self: &Rc<Self>,
        target_pane: Option<PaneId>,
        axis: SplitAxis,
        cwd: Option<PathBuf>,
        command: Option<(String, Vec<String>)>,
    ) -> io::Result<(WindowId, TabId, PaneId)> {
        let anchor = target_pane
            .or_else(|| self.workspace.borrow().active_pane_id())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no active pane"))?;
        let inherited_cwd = cwd.or_else(|| self.active_cwd(None, Some(anchor)));
        let new_pane = self.workspace.borrow_mut().reserve_pane_id();
        let engine = self.spawn_engine(new_pane, inherited_cwd, command)?;
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

    /// Writes a divider ratio from a user drag. Deliberately does not rebuild
    /// the window: rebuilding mid-drag would fight the gesture and lose the
    /// handle position.
    pub fn set_split_ratio(&self, tab_id: TabId, path: &[bool], ratio: f64) -> bool {
        self.workspace
            .borrow_mut()
            .set_split_ratio(tab_id, path, ratio)
    }

    /// Drag-to-reorder: moves a tab within its window and rebuilds the strip.
    pub fn reorder_tab(self: &Rc<Self>, win_id: WindowId, tab_id: TabId, new_index: usize) {
        if self
            .workspace
            .borrow_mut()
            .reorder_tab(win_id, tab_id, new_index)
            .is_ok()
        {
            self.sync_ui_window(win_id);
        }
    }

    /// Drag-to-redock (or reorder): moves `tab_id` into `dst_win_id` at
    /// `dst_index` (`None` appends at the end) without restarting any shell or
    /// multiplexer client, closing the source window if its last tab moved out.
    pub fn move_tab_to_window(
        self: &Rc<Self>,
        tab_id: TabId,
        dst_win_id: WindowId,
        dst_index: Option<usize>,
    ) -> bool {
        let Ok(src_win_id) = self
            .workspace
            .borrow_mut()
            .move_tab_to_window(tab_id, dst_win_id, dst_index)
        else {
            return false;
        };
        if src_win_id != dst_win_id {
            self.reconcile_or_close_ui_window(src_win_id);
        }
        self.sync_ui_window(dst_win_id);
        if src_win_id != dst_win_id
            && let Some(ui_win) = self.ui_windows.borrow().get(&dst_win_id).cloned()
        {
            ui_win.present();
        }
        true
    }

    /// Tears an entire tab out of a multi-tab window into a new top-level window
    /// (used when dragging a tab outside all Mutiny windows or invoking Move to
    /// New Window on a tab).
    pub fn detach_tab(self: &Rc<Self>, tab_id: TabId) -> Result<WindowId, TopologyError> {
        let src_win = self
            .workspace
            .borrow()
            .windows()
            .iter()
            .find(|w| w.tabs.iter().any(|t| t.id == tab_id))
            .map(|w| w.id);
        let new_win = self.workspace.borrow_mut().detach_tab(tab_id)?;
        if let Some(old_win) = src_win {
            self.reconcile_or_close_ui_window(old_win);
        }
        self.ensure_ui_window(new_win);
        self.sync_ui_window(new_win);
        if let Some(ui_win) = self.ui_windows.borrow().get(&new_win).cloned() {
            ui_win.present();
        }
        Ok(new_win)
    }

    pub fn close_pane(self: &Rc<Self>, pane_id: PaneId) {
        self.save_before_last_window_goes();
        let src_win = self.workspace.borrow().locate_pane(pane_id).map(|(w, _)| w);
        let _dropped = self.workspace.borrow_mut().close_pane(pane_id);
        if let Some(win_id) = src_win {
            self.reconcile_or_close_ui_window(win_id);
        }
        self.quit_if_empty();
    }

    /// Closes a whole tab by closing its panes, so the same cascade applies:
    /// the last pane removes the tab, and the last tab removes the window.
    pub fn close_tab(self: &Rc<Self>, win_id: WindowId, tab_id: TabId) -> bool {
        let panes: Vec<PaneId> = {
            let ws = self.workspace.borrow();
            let Some(wm) = ws.window(win_id) else {
                return false;
            };
            let Some(tab) = wm.tabs.iter().find(|t| t.id == tab_id) else {
                return false;
            };
            tab.root.leaves()
        };
        if panes.is_empty() {
            return false;
        }
        for pid in panes {
            self.close_pane(pid);
        }
        true
    }

    /// Marks a window as the quick terminal: a WM close dismisses it instead
    /// of ending its shell.
    fn prepare_quick_window(&self, win_id: WindowId) {
        if let Some(win) = self.workspace.borrow_mut().window_mut(win_id) {
            win.quick = true;
        }
        if let Some(ui) = self.ui_windows.borrow().get(&win_id).cloned() {
            ui.set_hide_on_close(true);
        }
    }

    pub fn quick_visible(&self) -> bool {
        self.quick_visible.get()
    }

    /// A WM close dismissed the quick terminal. The window and its shell stay.
    pub fn mark_quick_dismissed(&self) {
        self.quick_visible.set(false);
    }

    /// Summons or dismisses the quick terminal, returning its window and
    /// whether it is now showing.
    ///
    /// The shell keeps running while dismissed, so this is a scratch pane you
    /// can reach with one keystroke rather than a new session each time. It is
    /// a plain shell, not the preferred multiplexer: a scratch pane should not
    /// leave a session behind.
    ///
    /// It is NOT pinned above other windows. GTK4 has no keep-above API and
    /// Wayland has no protocol for it, so summoning raises the window and
    /// dismissing hides it, and that is the whole of the stacking behaviour.
    pub fn toggle_quick_terminal(self: &Rc<Self>) -> io::Result<(WindowId, bool)> {
        let existing = self
            .workspace
            .borrow()
            .windows()
            .iter()
            .find(|w| w.quick)
            .map(|w| w.id);
        let win_id = match existing {
            Some(id) => id,
            None => {
                let (id, _tab, _pane) = self.create_window(None)?;
                self.prepare_quick_window(id);
                self.quick_visible.set(false);
                id
            }
        };
        let show = !self.quick_visible.get();
        self.quick_visible.set(show);
        if let Some(ui) = self.ui_windows.borrow().get(&win_id).cloned() {
            if show {
                ui.set_hide_on_close(true);
                ui.gtk_window().set_visible(true);
                ui.gtk_window().present();
            } else {
                ui.hide();
            }
        }
        if show {
            self.workspace.borrow_mut().set_active_window(win_id);
        }
        Ok((win_id, show))
    }

    /// Closes a whole window and every pane in it.
    pub fn close_window(self: &Rc<Self>, win_id: WindowId) -> bool {
        if self.workspace.borrow().window(win_id).is_none() {
            return false;
        }
        if let Some(ui) = self.ui_windows.borrow().get(&win_id).cloned() {
            ui.close_programmatically();
        }
        self.handle_window_closed(win_id);
        true
    }

    /// Saves the layout before a mutation that could empty the workspace.
    ///
    /// The exit save in `main` runs after the last window has been removed from
    /// the model, so on a normal quit it captured an EMPTY workspace and wiped
    /// the bundle. Closing the last window is how people actually quit, so that
    /// path silently lost the layout every time.
    fn save_before_last_window_goes(&self) {
        if self.workspace.borrow().windows().len() == 1 {
            let _ = self.save_session();
        }
    }

    pub fn handle_window_closed(&self, win_id: WindowId) {
        if self
            .workspace
            .borrow()
            .window(win_id)
            .is_some_and(|w| w.quick)
        {
            self.quick_visible.set(false);
        }
        self.save_before_last_window_goes();
        self.ui_windows.borrow_mut().remove(&win_id);
        self.sessions_menus
            .borrow_mut()
            .retain(|(w, _)| *w != Some(win_id));
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
                // New Tab follows the preferred multiplexer. `none` keeps the
                // plain-shell behaviour. The IPC SpawnTab verb deliberately
                // stays plain: agents and scripts ask for a shell explicitly.
                let preferred = self.config.borrow().preferred_mux;
                match preferred {
                    Some(kind) => {
                        let _ =
                            self.new_session(Some(kind), None, mux::SessionMode::Tab, active_win);
                    }
                    None => {
                        let _ = self.create_tab(active_win, None);
                    }
                }
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
                } else if self.unread_peer_alert_count() > 0
                    && let Some(win_id) = active_win
                    && let Some(ui_win) = self.ui_windows.borrow().get(&win_id).cloned()
                {
                    ui_win.present();
                    ui_win.popup_limb_popover();
                }
            }
            ActionId::ClearPeerAlerts => {
                self.clear_peer_alerts();
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
            ActionId::Settings => {
                if let Some(win_id) = active_win
                    && let Some(ui_win) = self.ui_windows.borrow().get(&win_id).cloned()
                {
                    ui_win.show_settings_dialog();
                }
            }
            ActionId::QuickTerminal => {
                if let Err(err) = self.toggle_quick_terminal() {
                    eprintln!("could not toggle the quick terminal: {err}");
                }
            }
            ActionId::SaveSession => {
                if let Err(err) = self.save_session() {
                    eprintln!("failed to save session: {err}");
                }
            }
            ActionId::CloseTab => {
                let target = active_win.and_then(|w| {
                    self.workspace
                        .borrow()
                        .window(w)
                        .map(|wm| (w, wm.active_tab))
                });
                if let Some((w, t)) = target {
                    self.close_tab(w, t);
                }
            }
            ActionId::CloseWindow => {
                if let Some(w) = active_win {
                    self.close_window(w);
                }
            }
            ActionId::NewSession => {
                if let Some(win_id) = active_win
                    && let Some(ui_win) = self.ui_windows.borrow().get(&win_id).cloned()
                {
                    ui_win.show_new_session_dialog();
                }
            }
            ActionId::PairHaven => {
                if let Some(win_id) = active_win
                    && let Some(ui_win) = self.ui_windows.borrow().get(&win_id).cloned()
                {
                    ui_win.show_pair_haven_dialog();
                }
            }
            ActionId::InsertPeerTotp => {
                if let Some(pid) = active_pane
                    && let Err(err) = self.trigger_insert_peer_totp(active_win, pid)
                {
                    eprintln!("warning: insert-peer-totp: {err}");
                }
            }
            ActionId::SessionPicker => {
                if let Some(win_id) = active_win
                    && let Some(ui_win) = self.ui_windows.borrow().get(&win_id).cloned()
                {
                    ui_win.show_session_picker();
                }
            }
            ActionId::OpenSessionGrid => {
                if let Err(err) = self.open_session_grid(Vec::new(), None, None) {
                    eprintln!("warning: open-session-grid: {err}");
                }
            }
            ActionId::HandoffToPeer => {
                if let Err(err) = self.begin_handoff_to_peer(active_pane, None, None, None) {
                    eprintln!("warning: handoff-to-peer: {err}");
                }
            }
            ActionId::PresentToPeer => {
                if let Some(win_id) = active_win
                    && let Some(ui_win) = self.ui_windows.borrow().get(&win_id).cloned()
                {
                    ui_win.show_present_to_peer_dialog();
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

    /// The live view for a window, if one exists. The controller owns these
    /// instances; a separately constructed `MutinyWindow` is not the one the
    /// tick refreshes.
    pub fn ui_window(&self, win_id: WindowId) -> Option<MutinyWindow> {
        self.ui_windows.borrow().get(&win_id).cloned()
    }

    fn ensure_ui_window(self: &Rc<Self>, win_id: WindowId) {
        if self.ui_windows.borrow().contains_key(&win_id) {
            return;
        }
        let Some(app) = self.gtk_app.borrow().clone() else {
            return;
        };
        let ui_win = MutinyWindow::new(&app, win_id, Rc::clone(self));
        // A window opened while an agent is already driving must show the
        // active icon at once, not wait for the next transition.
        ui_win.set_agent_active(self.agent_active());
        self.ui_windows.borrow_mut().insert(win_id, ui_win.clone());
        ui_win.present();
    }

    fn reconcile_or_close_ui_window(&self, win_id: WindowId) {
        let still_exists = self.workspace.borrow().window(win_id).is_some();
        if still_exists {
            self.sync_ui_window(win_id);
        } else if let Some(ui_win) = self.ui_windows.borrow_mut().remove(&win_id) {
            self.sessions_menus
                .borrow_mut()
                .retain(|(w, _)| *w != Some(win_id));
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
        self.sync_shade_poll_state();
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
                    EngineNotification::DesktopNotification { title, body } => {
                        self.dispatch_outbound_notification(&OutboundNotification {
                            title,
                            body,
                            priority: NotificationPriority::Default,
                        });
                        if let Some((wid, _)) = self.workspace.borrow().locate_pane(pid)
                            && !windows_needing_chrome_refresh.contains(&wid)
                        {
                            windows_needing_chrome_refresh.push(wid);
                        }
                    }
                    EngineNotification::CommandFinished(record) => {
                        let threshold_secs = self.config.borrow().long_command_secs;
                        if threshold_secs > 0
                            && record.duration_ms.unwrap_or(0)
                                >= threshold_secs.saturating_mul(1_000)
                            && !self.is_pane_focused_by_operator(pid)
                        {
                            let label = {
                                let mut ws = self.workspace.borrow_mut();
                                if let Some(eng) = ws.pane_mut(pid) {
                                    eng.mark_osc_notification_attention();
                                    let clean = mux::sanitize_text(eng.title(), 32);
                                    if clean.is_empty() {
                                        String::from("mutiny")
                                    } else {
                                        clean
                                    }
                                } else {
                                    String::from("mutiny")
                                }
                            };
                            let secs =
                                (record.duration_ms.unwrap_or(0) / 1_000).max(threshold_secs);
                            let code = record.exit_code.unwrap_or(0);
                            let (title, priority) = if code == 0 {
                                (
                                    String::from("Command finished (exit 0)"),
                                    NotificationPriority::Default,
                                )
                            } else {
                                (
                                    format!("Command failed (exit {code})"),
                                    NotificationPriority::High,
                                )
                            };
                            // Strictly session/tab/pane label + elapsed duration;
                            // NEVER include `record.output` or any scrollback text.
                            let body = format!("{label} · pane {} · {secs}s", pid.0);
                            self.dispatch_outbound_notification(&OutboundNotification {
                                title,
                                body,
                                priority,
                            });
                        }
                        if let Some((wid, _)) = self.workspace.borrow().locate_pane(pid)
                            && !windows_needing_chrome_refresh.contains(&wid)
                        {
                            windows_needing_chrome_refresh.push(wid);
                        }
                    }
                    EngineNotification::TitleChanged(_)
                    | EngineNotification::Bell
                    | EngineNotification::PromptReached { .. } => {
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
                    EngineNotification::ChildExited(code) => {
                        panes_exited.push((pid, code));
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
        self.advance_discoveries();
        self.advance_mux_commands();
        self.poll_inbound_pairing();
        self.poll_peer_client();
        self.poll_unfocused_consent_relay();
        self.refresh_agent_icon();
        self.refresh_sessions_periodically();
        self.expire_pending_consents();

        for (pid, reported_code) in panes_exited {
            let failed_mux = {
                let ws = self.workspace.borrow();
                ws.pane(pid).and_then(|eng| {
                    let code = reported_code.or_else(|| eng.last_child_exit_code())?;
                    if code == 0 {
                        return None;
                    }
                    let mux_ref = eng.mux_session()?.clone();
                    let src_win = ws.locate_pane(pid).map(|(w, _)| w);
                    let detail = eng
                        .snapshot(Some(8))
                        .lines
                        .into_iter()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .map(|l| mux::sanitize_text(l.trim(), 160))
                        .unwrap_or_default();
                    Some((src_win, mux_ref, code, detail))
                })
            };
            self.close_pane(pid);
            if let Some((src_win, mux_ref, code, detail)) = failed_mux {
                let clean_name = mux::sanitize_text(&mux_ref.name, 64);
                let msg = if detail.is_empty() {
                    format!(
                        "{} session {:?} exited with status {code}",
                        mux_ref.kind.label(),
                        clean_name
                    )
                } else {
                    format!(
                        "{} session {:?} exited (status {code}): {detail}",
                        mux_ref.kind.label(),
                        clean_name
                    )
                };
                self.surface_window_notice(src_win, msg);
            }
        }

        self.accept_ipc_clients();
    }

    /// Services both carriers. The operator socket assigns `Operator` to a
    /// same-uid peer; the agent socket assigns the mode the operator pinned at
    /// launch. Neither reads a mode from the request.
    fn accept_ipc_clients(self: &Rc<Self>) {
        self.accept_carrier(&self.ipc_listener, ConsentMode::Operator, "ctl", None);
        let agent_mode = self.agent_mode.get();
        self.accept_carrier(&self.agent_listener, agent_mode, "mcp-agent", None);
        self.accept_http_peer_carriers();
    }

    /// Services one carrier per HTTP peer token. Each assigns the identity its
    /// token was verified for, so two peers are distinct in the audit trail and
    /// in the grant set. The mode is the agent carrier's, never `Operator`.
    fn accept_http_peer_carriers(self: &Rc<Self>) {
        let mut accepted_streams = Vec::new();
        {
            let peers = self.http_peers.borrow();
            for peer in peers.iter() {
                loop {
                    match peer.listener.accept() {
                        Ok((stream, _)) => accepted_streams.push((
                            stream,
                            peer.mode,
                            peer.label.clone(),
                            peer.identity,
                        )),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(_) => break,
                    }
                }
            }
        }
        for (stream, mode, label, identity) in accepted_streams {
            self.handle_accepted(stream, mode, &label, Some(identity));
        }
    }

    fn accept_carrier(
        self: &Rc<Self>,
        cell: &RefCell<Option<UnixListener>>,
        carrier: ConsentMode,
        default_label: &str,
        identity_override: Option<PeerIdentity>,
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
                Ok((stream, _)) => {
                    self.handle_accepted(stream, carrier, default_label, identity_override)
                }
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
        identity_override: Option<PeerIdentity>,
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
        // A token-peer carrier assigns the identity it verified the token for.
        // The kernel check above still applies: the relaying process must be
        // same-uid. The remote client never chooses this identity, it only
        // presents a token, and the token-to-carrier mapping is fixed at launch.
        let peer = identity_override.unwrap_or_else(|| PeerIdentity::from_credentials(&cred));

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

        // Strict operator-only invariant: a Peer carrier (HTTP MCP or agent
        // socket) must never dump live TOTP codes into a terminal pane.
        if matches!(
            env_req.request,
            IpcRequest::RunAction {
                action: ActionId::InsertPeerTotp,
                ..
            }
        ) && carrier.carrier_role() != CarrierRole::Operator
        {
            self.record_audit(
                &client_label,
                &env_req.request,
                AuditDecision::Denied,
                carrier,
                peer,
            );
            return Some(IpcResponse::err(
                "insert-peer-totp is restricted to the local Operator; Peer carriers are refused",
            ));
        }

        let gui_present = !self.ui_windows.borrow().is_empty() || self.has_connected_haven_signer();
        // Tier 3 first: an installed policy substitutes for the prompt an ask
        // carrier would raise. It never escalates a carrier, so it is consulted
        // only for ask + Act and returns None otherwise.
        self.dispatch_peer.set(peer);
        self.dispatch_carrier.set(carrier);
        *self.dispatch_client_label.borrow_mut() = client_label.clone();
        let tier = env_req.request.consent_tier();
        let verb = env_req.request.verb_name();
        let pins = env_req.request.pin_fields();
        let now = now_ms();
        let policy_hit = {
            let policies = self.policies.borrow();
            let mut uses = self.policy_uses.borrow_mut();
            for history in uses.values_mut() {
                crate::policy::prune_uses(history, now);
            }
            crate::policy::policy_decision(
                crate::policy::PolicyCall {
                    tier,
                    carrier,
                    identity: peer,
                    verb,
                    pins: &pins,
                    now_ms: now,
                },
                &policies,
                &uses,
            )
        };
        if let Some(ConsentOutcome::Allow(decision)) = policy_hit {
            // Record the use against the covering policy so the ceiling holds.
            let covering = self
                .policies
                .borrow()
                .iter()
                .find(|p| {
                    p.peer == peer
                        && crate::policy::policy_match(
                            p,
                            verb,
                            &pins,
                            now,
                            &self
                                .policy_uses
                                .borrow()
                                .get(&p.id)
                                .cloned()
                                .unwrap_or_default(),
                        ) == crate::policy::PolicyMatch::Covered
                })
                .map(|p| p.id);
            if let Some(id) = covering {
                self.policy_uses
                    .borrow_mut()
                    .entry(id)
                    .or_default()
                    .push_back(now);
            }
            self.record_audit(&client_label, &env_req.request, decision, carrier, peer);
            return self.execute_ipc_request(env_req.request, raw_stream);
        }
        let outcome = {
            let empty_grants = HashSet::new();
            let guard = self.session_approved_clients.borrow();
            let grants = if env_req.request.consent_tier() == crate::action::ConsentTier::Act {
                &*guard
            } else {
                &empty_grants
            };
            consent_outcome(
                env_req.request.consent_tier(),
                carrier,
                peer,
                grants,
                gui_present,
            )
        };

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
                let now_inst = Instant::now();
                self.pending_consents
                    .borrow_mut()
                    .push_back(PendingConsent {
                        id: cid,
                        client: client_label,
                        request: env_req.request,
                        carrier,
                        peer,
                        created_at: now_inst,
                        deadline: now_inst + Duration::from_secs(25),
                        relayed_to_haven: false,
                        remote_request_id: None,
                        remote_sandbox_id: None,
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
                let mut summary = self.workspace.borrow().summary();
                if summary.windows.iter().any(|w| w.quick) {
                    summary.quick_visible = Some(self.quick_visible.get());
                }
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
            IpcRequest::NewSession {
                kind,
                name,
                mode,
                window,
            } => match self.new_session(kind, name, mode, window) {
                Ok((w, t, p)) => Some(ids_ok(Some(w), Some(t), Some(p))),
                Err(err) => Some(IpcResponse::err(err.to_string())),
            },
            IpcRequest::ProposePolicy {
                description,
                verbs,
                arg_constraints,
                max_calls_per_minute,
                expires_in_minutes,
            } => {
                let peer = self.dispatch_peer.get();
                match self.propose_policy(
                    peer,
                    description,
                    verbs,
                    arg_constraints,
                    max_calls_per_minute,
                    expires_in_minutes,
                ) {
                    Ok(id) => {
                        let mut resp = IpcResponse::empty_ok();
                        if let IpcResponse::Ok { policy_id, .. } = &mut resp {
                            *policy_id = Some(id);
                        }
                        Some(resp)
                    }
                    Err(err) => Some(IpcResponse::err(err)),
                }
            }
            IpcRequest::ListPolicies => {
                let mut resp = IpcResponse::empty_ok();
                if let IpcResponse::Ok {
                    policies,
                    pending_policies,
                    ..
                } = &mut resp
                {
                    *policies = Some(self.installed_policies());
                    *pending_policies = Some(self.pending_policies());
                }
                Some(resp)
            }
            IpcRequest::InstallPolicy { id } => match self.install_policy(id) {
                Ok(()) => Some(IpcResponse::empty_ok()),
                Err(err) => Some(IpcResponse::err(err)),
            },
            IpcRequest::RevokePolicy { id } => {
                if self.revoke_policy(id) {
                    Some(IpcResponse::empty_ok())
                } else {
                    Some(IpcResponse::err(format!("no such policy {id}")))
                }
            }
            IpcRequest::ListPeers => {
                let mut resp = IpcResponse::empty_ok();
                if let IpcResponse::Ok { peers, .. } = &mut resp {
                    *peers = Some(self.list_peers());
                }
                Some(resp)
            }
            IpcRequest::PairHaven {
                label,
                url,
                token,
                enroll_signer,
            } => {
                let Ok(cloned) = raw_stream.try_clone() else {
                    return Some(IpcResponse::err("could not clone the request stream"));
                };
                match self.begin_pair_haven(label, url, token, enroll_signer, Some(cloned)) {
                    Ok(()) => None,
                    Err(err) => Some(IpcResponse::err(err)),
                }
            }
            IpcRequest::EnrollHavenSigner { id_or_label } => {
                let Ok(cloned) = raw_stream.try_clone() else {
                    return Some(IpcResponse::err("could not clone the request stream"));
                };
                match self.begin_enroll_haven_signer(id_or_label, Some(cloned)) {
                    Ok(()) => None,
                    Err(err) => Some(IpcResponse::err(err)),
                }
            }
            IpcRequest::RevokePeer { id_or_label } => match self.revoke_peer(&id_or_label) {
                Ok(()) => Some(IpcResponse::empty_ok()),
                Err(err) => Some(IpcResponse::err(err)),
            },
            IpcRequest::ListPeerAlerts => {
                let mut resp = IpcResponse::empty_ok();
                if let IpcResponse::Ok { peer_alerts, .. } = &mut resp {
                    *peer_alerts = Some(self.list_peer_alerts());
                }
                Some(resp)
            }
            IpcRequest::ClearPeerAlerts => {
                self.clear_peer_alerts();
                Some(IpcResponse::empty_ok())
            }
            IpcRequest::SessionTree { refresh } => {
                if !refresh {
                    return Some(self.session_tree_response());
                }
                if self.pending_tree_reply.borrow().is_some() {
                    return Some(self.session_tree_response());
                }
                let Ok(cloned) = raw_stream.try_clone() else {
                    return Some(IpcResponse::err("could not clone the request stream"));
                };
                let awaiting_limbs = self.refresh_haven_limb_inventories();
                self.tree_awaiting_limbs.set(awaiting_limbs);
                *self.pending_tree_reply.borrow_mut() = Some(cloned);
                if !self.discovery_in_flight() {
                    self.begin_discovery(vec![None], true, None);
                }
                self.maybe_finish_tree_reply();
                None
            }
            IpcRequest::OpenSessionGrid {
                sessions,
                kind,
                host,
            } => match self.open_session_grid(sessions, kind, host.as_deref()) {
                Ok((w, t, p)) => {
                    let mut resp = ids_ok(Some(w), Some(t), Some(p));
                    if let IpcResponse::Ok { workspace, .. } = &mut resp {
                        *workspace = Some(self.workspace.borrow().summary());
                    }
                    Some(resp)
                }
                Err(err) => Some(IpcResponse::err(err.to_string())),
            },
            IpcRequest::HandoffToPeer {
                pane,
                peer,
                profile_id,
            } => {
                let Ok(cloned) = raw_stream.try_clone() else {
                    return Some(IpcResponse::err("could not clone the request stream"));
                };
                match self.begin_handoff_to_peer(pane, peer, profile_id, Some(cloned)) {
                    Ok(()) => None,
                    Err(err) => Some(IpcResponse::err(err)),
                }
            }
            IpcRequest::PresentToPeer {
                peer,
                url,
                path,
                text,
                clipboard,
                profile_id,
                caption,
            } => {
                let Ok(cloned) = raw_stream.try_clone() else {
                    return Some(IpcResponse::err("could not clone the request stream"));
                };
                match self.begin_present_to_peer(
                    peer,
                    url,
                    path,
                    text,
                    clipboard,
                    profile_id,
                    caption,
                    Some(cloned),
                ) {
                    Ok(()) => None,
                    Err(err) => Some(IpcResponse::err(err)),
                }
            }
            IpcRequest::EndSession { kind, name, host } => {
                let Ok(cloned) = raw_stream.try_clone() else {
                    return Some(IpcResponse::err("could not clone the request stream"));
                };
                match self.end_session_with_reply(kind, &name, host.as_deref(), Some(cloned)) {
                    Ok(immediate) => immediate,
                    Err(err) => Some(IpcResponse::err(err.to_string())),
                }
            }
            IpcRequest::RenameSession {
                kind,
                name,
                new_name,
                host,
            } => {
                let Ok(cloned) = raw_stream.try_clone() else {
                    return Some(IpcResponse::err("could not clone the request stream"));
                };
                match self.rename_session_with_reply(
                    kind,
                    &name,
                    &new_name,
                    host.as_deref(),
                    Some(cloned),
                ) {
                    Ok(immediate) => immediate,
                    Err(err) => Some(IpcResponse::err(err.to_string())),
                }
            }
            IpcRequest::ToggleQuick => match self.toggle_quick_terminal() {
                Ok((window, visible)) => {
                    let mut resp = IpcResponse::empty_ok();
                    if let IpcResponse::Ok {
                        window: w,
                        visible: v,
                        ..
                    } = &mut resp
                    {
                        *w = Some(window);
                        *v = Some(visible);
                    }
                    Some(resp)
                }
                Err(err) => Some(IpcResponse::err(format!(
                    "could not toggle the quick terminal: {err}"
                ))),
            },
            IpcRequest::SaveSession => match self.save_session() {
                Ok(()) => Some(IpcResponse::empty_ok()),
                Err(err) => Some(IpcResponse::err(format!("could not save session: {err}"))),
            },
            IpcRequest::CloseTab { window, tab } => {
                let ws = self.workspace.borrow();
                let win = window.or_else(|| ws.active_window_id());
                let Some(win) = win else {
                    return Some(IpcResponse::err("no active window"));
                };
                let tab = tab.or_else(|| ws.window(win).map(|w| w.active_tab));
                drop(ws);
                let Some(tab) = tab else {
                    return Some(IpcResponse::err("no active tab"));
                };
                if self.close_tab(win, tab) {
                    Some(IpcResponse::empty_ok())
                } else {
                    Some(IpcResponse::err(format!("no such tab {}", tab.0)))
                }
            }
            IpcRequest::CloseWindow { window } => {
                let win = window.or_else(|| self.workspace.borrow().active_window_id());
                let Some(win) = win else {
                    return Some(IpcResponse::err("no active window"));
                };
                if self.close_window(win) {
                    Some(IpcResponse::empty_ok())
                } else {
                    Some(IpcResponse::err(format!("no such window {}", win.0)))
                }
            }
            IpcRequest::ClosePane { pane } => {
                let pid = pane.or_else(|| self.workspace.borrow().active_pane_id());
                let Some(pid) = pid else {
                    return Some(IpcResponse::err("no active pane"));
                };
                if self.workspace.borrow().pane(pid).is_none() {
                    return Some(IpcResponse::err(format!("no such pane {}", pid.0)));
                }
                self.close_pane(pid);
                Some(IpcResponse::empty_ok())
            }
            IpcRequest::Quit => {
                if let Err(err) = self.save_session() {
                    return Some(IpcResponse::err(format!("could not save session: {err}")));
                }
                // Defer the quit so this reply is written before the loop stops.
                if let Some(app) = self.gtk_app.borrow().clone() {
                    gtk4::glib::idle_add_local_once(move || app.quit());
                }
                Some(IpcResponse::empty_ok())
            }
            IpcRequest::ListSessions {
                host,
                refresh,
                unattached_only,
            } => {
                // `host: None` is local only. Scanning configured remotes is the
                // GUI picker's job, where the human is the consent; an Observe
                // IPC call must not spend the user's SSH credentials.
                let targets = match host {
                    Some(host) => {
                        if let Err(err) = mux::validate_host(&host) {
                            return Some(IpcResponse::err(err));
                        }
                        vec![Some(host)]
                    }
                    None => vec![None],
                };
                let Ok(stream) = raw_stream.try_clone() else {
                    return Some(IpcResponse::err("could not clone the request stream"));
                };
                self.unattached_filter.set(unattached_only);
                self.begin_discovery(targets, refresh, Some(stream))
            }
            IpcRequest::OpenSession {
                kind,
                name,
                host,
                mode,
                window,
                pane,
            } => match self.open_session(kind, &name, host.as_deref(), mode, window, pane) {
                Ok((w, t, p)) => Some(ids_ok(Some(w), Some(t), Some(p))),
                Err(err) => Some(IpcResponse::err(err.to_string())),
            },
            IpcRequest::PresentMedia {
                pane,
                path,
                cols,
                rows,
            } => {
                let pid = pane.or_else(|| self.workspace.borrow().active_pane_id());
                let Some(pid) = pid else {
                    return Some(IpcResponse::err("no active pane"));
                };
                let data = match std::fs::read(&path) {
                    Ok(data) => data,
                    Err(err) => {
                        return Some(IpcResponse::err(format!(
                            "cannot read {}: {err}",
                            path.display()
                        )));
                    }
                };
                let result = self
                    .workspace
                    .borrow_mut()
                    .pane_mut(pid)
                    .ok_or_else(|| format!("pane {pid} not found"))
                    .and_then(|eng| eng.present_image(data, cols.unwrap_or(0), rows.unwrap_or(0)));
                match result {
                    Ok((image_id, placement_id)) => {
                        self.queue_redraw_pane(pid);
                        let mut resp = ids_ok(None, None, Some(pid));
                        if let IpcResponse::Ok {
                            image_id: img,
                            placement_id: plc,
                            ..
                        } = &mut resp
                        {
                            *img = Some(image_id);
                            *plc = Some(placement_id);
                        }
                        Some(resp)
                    }
                    Err(err) => Some(IpcResponse::err(err)),
                }
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
                let clean_msg = message
                    .as_deref()
                    .map(|m| mux::sanitize_text(m.trim(), 240))
                    .filter(|m| !m.is_empty());
                let carrier = self.dispatch_carrier.get();
                let caller = {
                    let raw = mux::sanitize_text(self.dispatch_client_label.borrow().trim(), 48);
                    if raw.is_empty() {
                        String::from("peer")
                    } else {
                        raw
                    }
                };
                let pane_notice = match (&clean_msg, carrier.carrier_role()) {
                    (Some(msg), CarrierRole::Peer) => Some(format!("[{caller}] {msg}")),
                    (Some(msg), CarrierRole::Operator) => Some(msg.clone()),
                    (None, _) => None,
                };
                {
                    let mut ws = self.workspace.borrow_mut();
                    let Some(eng) = ws.pane_mut(pid) else {
                        return Some(IpcResponse::err(format!("pane {pid} not found")));
                    };
                    eng.set_agent_notice(pane_notice);
                }
                if let Some(msg) = clean_msg {
                    if carrier.carrier_role() == CarrierRole::Peer {
                        self.push_peer_alert(
                            format!("peer:{caller}"),
                            caller.clone(),
                            PeerAlertKind::PeerNotice,
                            None,
                            format!("Notice from {caller}"),
                            msg.clone(),
                            now_ms(),
                            true,
                        );
                    }
                    // Avoid ping-pong echo back to an HTTP token peer that pushed
                    // this notice; local operator / stdio MCP notices mirror via
                    // CompositeNotifier.
                    if !self.dispatch_peer.get().is_token_peer() {
                        self.dispatch_outbound_notification(&OutboundNotification {
                            title: format!("Notice on pane {}", pid.0),
                            body: msg,
                            priority: NotificationPriority::Default,
                        });
                    }
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

    /// Refreshes open session pickers. Collects window clones first, exactly as
    /// `refresh_search_bars` does, because each window call re-borrows state.
    /// The window itself decides whether the generation moved, so this is cheap
    /// when nothing changed.
    pub fn refresh_session_pickers(&self) {
        let generation = self.cache_gen.get();
        if self.sessions_menu_gen.get() != generation {
            self.sessions_menu_gen.set(generation);
            let inventory = self.session_inventory();
            let menus: Vec<gtk4::gio::Menu> = self
                .sessions_menus
                .borrow()
                .iter()
                .map(|(_, m)| m.clone())
                .collect();
            for menu in menus {
                crate::action::populate_sessions_menu(&menu, &inventory);
            }
        }
        let wins: Vec<MutinyWindow> = self.ui_windows.borrow().values().cloned().collect();
        for w in wins {
            w.refresh_session_picker(generation);
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

    fn poll_inbound_pairing(&self) {
        let now = Instant::now();
        let expired = {
            let mut slot = self.pending_pairing.borrow_mut();
            if slot.as_ref().is_some_and(|p| now >= p.deadline) {
                slot.take()
            } else {
                None
            }
        };
        if let Some(item) = expired {
            let _ = item.reply_tx.send(PairingOutcome::Denied);
            self.refresh_all_window_chromes();
        }

        if self.pending_pairing.borrow().is_some() {
            return;
        }
        let next_req = self
            .pairing_rx
            .borrow()
            .as_ref()
            .and_then(|rx| rx.try_recv().ok());
        let Some(req) = next_req else {
            return;
        };

        let can_prompt = !self.ui_windows.borrow().is_empty() || self.allow_headless_pairing.get();
        if !can_prompt {
            let _ = req.reply_tx.send(PairingOutcome::Denied);
            return;
        }

        let id = self.next_pairing_id.get();
        self.next_pairing_id.set(id + 1);
        *self.pending_pairing.borrow_mut() = Some(PendingInboundPairing {
            id,
            client_name: req.client_name,
            remote_addr: req.remote_addr,
            deadline: now + PAIRING_TIMEOUT,
            reply_tx: req.reply_tx,
        });
        self.refresh_all_window_chromes();
    }

    fn poll_peer_client(self: &Rc<Self>) {
        let mut events = Vec::new();
        if let Some(client) = self.peer_client.borrow().as_ref() {
            while let Some(ev) = client.try_recv() {
                events.push(ev);
            }
        }
        if events.is_empty() {
            return;
        }
        let mut refresh_ui = false;
        for ev in events {
            match ev {
                PeerClientEvent::StatusChanged {
                    peer_id,
                    status,
                    connected_at,
                } => {
                    if let Some(pid) = peer_id {
                        let prev = self
                            .outbound_statuses
                            .borrow_mut()
                            .insert(pid.clone(), status.clone());
                        let had_ever_connected = self
                            .peer_store
                            .borrow()
                            .outbound_peers
                            .iter()
                            .find(|p| p.id == pid)
                            .is_some_and(|p| p.last_connected_at.is_some());
                        if let Some(ts) = connected_at
                            && let Some(entry) = self
                                .peer_store
                                .borrow_mut()
                                .outbound_peers
                                .iter_mut()
                                .find(|p| p.id == pid)
                        {
                            entry.last_connected_at = Some(ts);
                        }
                        if matches!(status, OutboundLimbStatus::Offline { .. })
                            && self.limb_inventories.borrow_mut().remove(&pid).is_some()
                        {
                            self.bump_cache_gen();
                        }
                        match (&prev, &status) {
                            (
                                Some(OutboundLimbStatus::Connected { .. }),
                                OutboundLimbStatus::Offline {
                                    label, last_error, ..
                                },
                            ) => {
                                self.mark_peer_alerts_read_by_kind(
                                    &pid,
                                    PeerAlertKind::LimbOffline,
                                );
                                self.push_peer_alert(
                                    pid,
                                    label.clone(),
                                    PeerAlertKind::LimbOffline,
                                    None,
                                    format!("Haven limb '{label}' disconnected"),
                                    last_error
                                        .clone()
                                        .unwrap_or_else(|| String::from("connection lost")),
                                    now_ms(),
                                    true,
                                );
                            }
                            (
                                Some(OutboundLimbStatus::Offline { .. }),
                                OutboundLimbStatus::Connected { label, .. },
                            ) if had_ever_connected => {
                                self.mark_peer_alerts_read_by_kind(
                                    &pid,
                                    PeerAlertKind::LimbOffline,
                                );
                                self.push_peer_alert(
                                    pid,
                                    label.clone(),
                                    PeerAlertKind::LimbReconnected,
                                    None,
                                    format!("Haven limb '{label}' reconnected"),
                                    String::from("tunnel restored"),
                                    now_ms(),
                                    false,
                                );
                            }
                            (_, OutboundLimbStatus::Connected { .. }) => {
                                self.mark_peer_alerts_read_by_kind(
                                    &pid,
                                    PeerAlertKind::LimbOffline,
                                );
                            }
                            _ => {}
                        }
                    }
                    refresh_ui = true;
                }
                PeerClientEvent::PairCompleted { req_id, result } => {
                    *self.pairing_outbound_label.borrow_mut() = None;
                    let reply_stream = self.pending_pair_replies.borrow_mut().remove(&req_id);
                    match result {
                        Ok((peer, status)) => {
                            let summary = OutboundPeerSummary {
                                id: peer.id.clone(),
                                label: peer.label.clone(),
                                url: peer.url.clone(),
                                inbound_notify_scope: peer.inbound_notify_scope,
                                paired_at: peer.paired_at,
                                last_connected_at: peer.last_connected_at,
                                has_signing_key: peer.signing_secret_b64.is_some(),
                                signing_key_id: peer.signing_key_id.clone(),
                                rate_limited_count: 0,
                                unread_alert_count: 0,
                                shade_status: None,
                                status: status.clone(),
                            };
                            let save_res = {
                                let mut store = self.peer_store.borrow_mut();
                                store
                                    .outbound_peers
                                    .retain(|p| p.label != peer.label && p.id != peer.id);
                                store.outbound_peers.push(peer.clone());
                                peers::save(&self.peers_path.borrow(), &store)
                            };
                            match save_res {
                                Ok(()) => {
                                    self.outbound_statuses.borrow_mut().insert(peer.id, status);
                                    if let Some(mut stream) = reply_stream {
                                        let mut resp = IpcResponse::empty_ok();
                                        if let IpcResponse::Ok { outbound_peer, .. } = &mut resp {
                                            *outbound_peer = Some(summary);
                                        }
                                        let _ = write_response(&mut stream, &resp);
                                    }
                                }
                                Err(err) => {
                                    let msg = format!("failed to persist peers.json: {err}");
                                    if let Some(mut stream) = reply_stream {
                                        let _ = write_response(&mut stream, &IpcResponse::err(msg));
                                    } else {
                                        eprintln!("warning: {msg}");
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            if let Some(mut stream) = reply_stream {
                                let _ = write_response(&mut stream, &IpcResponse::err(err));
                            } else {
                                eprintln!("warning: outbound Haven pairing failed: {err}");
                            }
                        }
                    }
                    refresh_ui = true;
                }
                PeerClientEvent::SignerEnrolled { req_id, result } => {
                    let reply_stream = self.pending_pair_replies.borrow_mut().remove(&req_id);
                    match result {
                        Ok((peer_id, secret_b64, key_id)) => {
                            let mut updated_peers_opt = None;
                            let mut summary_opt = None;
                            let save_res = {
                                let mut store = self.peer_store.borrow_mut();
                                if let Some(entry) =
                                    store.outbound_peers.iter_mut().find(|p| p.id == peer_id)
                                {
                                    entry.signing_secret_b64 = Some(secret_b64);
                                    entry.signing_key_id = Some(key_id.clone());
                                    let status = self
                                        .outbound_statuses
                                        .borrow()
                                        .get(&entry.id)
                                        .cloned()
                                        .unwrap_or_else(|| OutboundLimbStatus::Offline {
                                            id: entry.id.clone(),
                                            label: entry.label.clone(),
                                            last_error: None,
                                        });
                                    let rl_count = self
                                        .outbound_rate_limiters
                                        .borrow()
                                        .get(&entry.id)
                                        .map(|b| b.rate_limited_count())
                                        .unwrap_or(0);
                                    let unread_count = self
                                        .peer_alerts
                                        .borrow()
                                        .iter()
                                        .filter(|a| a.unread && a.peer_id == entry.id)
                                        .count();
                                    let shade_note =
                                        self.peer_shade_statuses.borrow().get(&entry.id).cloned();
                                    summary_opt = Some(OutboundPeerSummary {
                                        id: entry.id.clone(),
                                        label: entry.label.clone(),
                                        url: entry.url.clone(),
                                        inbound_notify_scope: entry.inbound_notify_scope,
                                        paired_at: entry.paired_at,
                                        last_connected_at: entry.last_connected_at,
                                        has_signing_key: true,
                                        signing_key_id: Some(key_id),
                                        rate_limited_count: rl_count,
                                        unread_alert_count: unread_count,
                                        shade_status: shade_note,
                                        status,
                                    });
                                    let res = peers::save(&self.peers_path.borrow(), &store);
                                    if res.is_ok() {
                                        updated_peers_opt = Some(store.outbound_peers.clone());
                                    }
                                    res.map_err(|e| format!("failed to persist peers.json: {e}"))
                                } else {
                                    Err(format!(
                                        "outbound peer {peer_id:?} was removed before enrollment completed"
                                    ))
                                }
                            };
                            match save_res {
                                Ok(()) => {
                                    if let (Some(updated), Some(client)) =
                                        (updated_peers_opt, self.peer_client.borrow().as_ref())
                                    {
                                        let _ = client.set_peers(updated);
                                    }
                                    if let Some(mut stream) = reply_stream {
                                        let mut resp = IpcResponse::empty_ok();
                                        if let IpcResponse::Ok { outbound_peer, .. } = &mut resp {
                                            *outbound_peer = summary_opt;
                                        }
                                        let _ = write_response(&mut stream, &resp);
                                    }
                                }
                                Err(err) => {
                                    if let Some(mut stream) = reply_stream {
                                        let _ = write_response(&mut stream, &IpcResponse::err(err));
                                    } else {
                                        eprintln!("warning: {err}");
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            if let Some(mut stream) = reply_stream {
                                let _ = write_response(&mut stream, &IpcResponse::err(err));
                            } else {
                                eprintln!("warning: Haven signer enrollment failed: {err}");
                            }
                        }
                    }
                    refresh_ui = true;
                }
                PeerClientEvent::ConsentCompleted {
                    consent_id,
                    peer_id,
                    result,
                } => {
                    let pending_meta = {
                        let queue = self.pending_consents.borrow();
                        queue.iter().find(|c| c.id == consent_id).map(|c| {
                            (
                                c.remote_request_id.clone(),
                                c.remote_sandbox_id.clone(),
                                c.request.consent_tier(),
                            )
                        })
                    };
                    // First-human-wins race: if local Allow/Deny already resolved
                    // this PendingConsent, ignore the late remote response.
                    let Some((Some(exp_req_id), Some(exp_sandbox_id), tier)) = pending_meta else {
                        continue;
                    };
                    let secret_opt = self
                        .peer_store
                        .borrow()
                        .outbound_peers
                        .iter()
                        .find(|p| p.id == peer_id)
                        .and_then(|p| p.signing_secret_b64.clone());
                    let Some(secret_b64) = secret_opt else {
                        eprintln!(
                            "warning: Haven consent reply for {exp_req_id} rejected: missing signing key"
                        );
                        self.resolve_consent(consent_id, ConsentResolution::Deny);
                        continue;
                    };
                    let resolution = match result {
                        Ok(payload) => {
                            let expected = crypto::ExpectedEgressGrant {
                                request_id: exp_req_id.clone(),
                                sandbox_id: exp_sandbox_id,
                                host: String::from("mutiny.local"),
                                port: 1,
                            };
                            let verified = {
                                let mut cache = self.consent_replay_cache.borrow_mut();
                                crypto::verify_egress_decision(
                                    &secret_b64,
                                    &expected,
                                    &payload,
                                    now_ms(),
                                    &mut cache,
                                )
                            };
                            match verified {
                                crypto::VerifiedEgressDecision::Once => {
                                    ConsentResolution::AllowOnce
                                }
                                crypto::VerifiedEgressDecision::Session => {
                                    // Tier-3 / Tier-4 one-shot enforcement: Irreversible
                                    // and PolicyChange never record a session-wide grant.
                                    if matches!(
                                        tier,
                                        crate::action::ConsentTier::Irreversible
                                            | crate::action::ConsentTier::PolicyChange
                                    ) {
                                        ConsentResolution::AllowOnce
                                    } else {
                                        ConsentResolution::AllowSession
                                    }
                                }
                                crypto::VerifiedEgressDecision::Deny(reason) => {
                                    eprintln!(
                                        "warning: Haven consent denied or failed verification for {exp_req_id}: {reason}"
                                    );
                                    ConsentResolution::Deny
                                }
                            }
                        }
                        Err(err) => {
                            eprintln!("warning: Haven consent request {exp_req_id} failed: {err}");
                            ConsentResolution::Deny
                        }
                    };
                    self.resolve_consent(consent_id, resolution);
                }
                PeerClientEvent::AuthPromptChanged {
                    peer_id,
                    peer_label,
                    prompt,
                } => {
                    if self.in_flight_auth_answers.borrow().contains(&peer_id) {
                        // Suppress stale pre-submit liveness polls until the
                        // in-flight `answer_auth_prompt` call finishes.
                        continue;
                    }
                    match prompt {
                        Some(p) => {
                            let changed = {
                                let mut map = self.haven_auth_prompts.borrow_mut();
                                let c = map
                                    .get(&peer_id)
                                    .map(|(lbl, old)| lbl != &peer_label || old != &p)
                                    .unwrap_or(true);
                                map.insert(peer_id.clone(), (peer_label.clone(), p.clone()));
                                c
                            };
                            if changed {
                                self.mark_peer_alerts_read_by_kind(
                                    &peer_id,
                                    PeerAlertKind::AuthPrompt,
                                );
                                self.push_peer_alert(
                                    peer_id,
                                    peer_label.clone(),
                                    PeerAlertKind::AuthPrompt,
                                    None,
                                    format!("SSH auth needed on '{peer_label}'"),
                                    format!("Profile '{}' awaits credentials", p.label),
                                    now_ms(),
                                    true,
                                );
                                refresh_ui = true;
                            }
                        }
                        None => {
                            let removed = self
                                .haven_auth_prompts
                                .borrow_mut()
                                .remove(&peer_id)
                                .is_some();
                            let cleared = self
                                .mark_peer_alerts_read_by_kind(&peer_id, PeerAlertKind::AuthPrompt);
                            if removed || cleared {
                                refresh_ui = true;
                            }
                            self.dismissed_auth_prompts
                                .borrow_mut()
                                .retain(|(pid, _)| pid != &peer_id);
                        }
                    }
                }
                PeerClientEvent::AuthAnswerCompleted {
                    peer_id,
                    peer_label,
                    prompt,
                } => {
                    self.in_flight_auth_answers.borrow_mut().remove(&peer_id);
                    match prompt {
                        Some(p) => {
                            self.haven_auth_prompts
                                .borrow_mut()
                                .insert(peer_id.clone(), (peer_label.clone(), p.clone()));
                            self.mark_peer_alerts_read_by_kind(&peer_id, PeerAlertKind::AuthPrompt);
                            self.push_peer_alert(
                                peer_id,
                                peer_label.clone(),
                                PeerAlertKind::AuthPrompt,
                                None,
                                format!("SSH auth needed on '{peer_label}'"),
                                format!("Profile '{}' awaits credentials", p.label),
                                now_ms(),
                                true,
                            );
                            refresh_ui = true;
                        }
                        None => {
                            if self
                                .haven_auth_prompts
                                .borrow_mut()
                                .remove(&peer_id)
                                .is_some()
                            {
                                refresh_ui = true;
                            }
                            self.mark_peer_alerts_read_by_kind(&peer_id, PeerAlertKind::AuthPrompt);
                            self.dismissed_auth_prompts
                                .borrow_mut()
                                .retain(|(pid, _)| pid != &peer_id);
                        }
                    }
                }
                PeerClientEvent::RemoteConsentPromptChanged {
                    peer_id,
                    peer_label,
                    summary,
                } => match summary {
                    Some(s) => {
                        let changed = {
                            let mut map = self.remote_consent_prompts.borrow_mut();
                            let c = map.get(&peer_id).map(|old| old != &s).unwrap_or(true);
                            map.insert(peer_id.clone(), s.clone());
                            c
                        };
                        if changed {
                            self.mark_peer_alerts_read_by_kind(
                                &peer_id,
                                PeerAlertKind::PendingConsent,
                            );
                            self.push_peer_alert(
                                peer_id,
                                peer_label.clone(),
                                PeerAlertKind::PendingConsent,
                                None,
                                format!("Consent pending on '{peer_label}'"),
                                s,
                                now_ms(),
                                true,
                            );
                            refresh_ui = true;
                        }
                    }
                    None => {
                        let removed = self
                            .remote_consent_prompts
                            .borrow_mut()
                            .remove(&peer_id)
                            .is_some();
                        let cleared = self
                            .mark_peer_alerts_read_by_kind(&peer_id, PeerAlertKind::PendingConsent);
                        if removed || cleared {
                            refresh_ui = true;
                        }
                    }
                },
                PeerClientEvent::ShadeNotificationsPolled {
                    peer_id,
                    peer_label,
                    records,
                } => {
                    if self
                        .peer_shade_statuses
                        .borrow_mut()
                        .remove(&peer_id)
                        .is_some()
                    {
                        refresh_ui = true;
                    }
                    for rec in records {
                        if rec.removed {
                            let mut alerts = self.peer_alerts.borrow_mut();
                            for a in alerts.iter_mut() {
                                if a.unread
                                    && a.peer_id == peer_id
                                    && a.kind == PeerAlertKind::AndroidShade
                                    && a.package_name.as_deref() == Some(rec.package_name.as_str())
                                    && (a.timestamp_ms == rec.post_time_ms
                                        || (a.title == rec.title && a.body == rec.text))
                                {
                                    a.unread = false;
                                    refresh_ui = true;
                                }
                            }
                        } else {
                            let dup = self.peer_alerts.borrow().iter().any(|a| {
                                a.unread
                                    && a.peer_id == peer_id
                                    && a.kind == PeerAlertKind::AndroidShade
                                    && a.package_name.as_deref() == Some(rec.package_name.as_str())
                                    && a.timestamp_ms == rec.post_time_ms
                                    && a.title == rec.title
                                    && a.body == rec.text
                            });
                            if !dup {
                                let title = if rec.title.is_empty() {
                                    rec.app_name
                                        .clone()
                                        .unwrap_or_else(|| rec.package_name.clone())
                                } else {
                                    rec.title.clone()
                                };
                                self.push_peer_alert(
                                    peer_id.clone(),
                                    peer_label.clone(),
                                    PeerAlertKind::AndroidShade,
                                    Some(rec.package_name),
                                    title,
                                    rec.text,
                                    rec.post_time_ms,
                                    true,
                                );
                                refresh_ui = true;
                            }
                        }
                    }
                }
                PeerClientEvent::ShadeStatusChanged {
                    peer_id,
                    status_note,
                } => {
                    let mut map = self.peer_shade_statuses.borrow_mut();
                    match status_note {
                        Some(note) => {
                            if map.insert(peer_id, note.clone()).as_ref() != Some(&note) {
                                refresh_ui = true;
                            }
                        }
                        None => {
                            if map.remove(&peer_id).is_some() {
                                refresh_ui = true;
                            }
                        }
                    }
                }
                PeerClientEvent::TotpChoicesReady {
                    peer_id,
                    target_pane,
                    target_window,
                    secrets,
                } => {
                    let win_id =
                        target_window.or_else(|| self.workspace.borrow().active_window_id());
                    if let Some(wid) = win_id
                        && let Some(ui_win) = self.ui_windows.borrow().get(&wid).cloned()
                    {
                        ui_win.show_totp_chooser_dialog(&peer_id, target_pane, &secrets);
                    }
                }
                PeerClientEvent::TotpCodeReady { target_pane, code } => {
                    if let Some(eng) = self.workspace.borrow_mut().pane_mut(target_pane)
                        && let Err(err) = eng.write_to_pty(code.as_bytes())
                    {
                        eprintln!(
                            "warning: failed to write Haven TOTP code to pane {target_pane}: {err}"
                        );
                    }
                }
                PeerClientEvent::TotpError(err) => {
                    eprintln!("warning: Haven TOTP request failed: {err}");
                }
                PeerClientEvent::LimbInventoryUpdated { peer_id, inventory } => {
                    let changed = self
                        .limb_inventories
                        .borrow_mut()
                        .insert(peer_id, inventory.clone())
                        .as_ref()
                        != Some(&inventory);
                    if changed {
                        self.bump_cache_gen();
                        self.refresh_session_pickers();
                    }
                }
                PeerClientEvent::LimbInventoriesRefreshed => {
                    self.tree_awaiting_limbs.set(false);
                    self.maybe_finish_tree_reply();
                    self.bump_cache_gen();
                    self.refresh_session_pickers();
                }
                PeerClientEvent::HandoffCompleted { req_id, result } => {
                    let reply_stream = self.pending_handoff_replies.borrow_mut().remove(&req_id);
                    match result {
                        Ok(summary) => {
                            let _ = self.refresh_haven_limb_inventories();
                            if let Some(mut stream) = reply_stream {
                                let mut resp = IpcResponse::empty_ok();
                                if let IpcResponse::Ok { handoff, .. } = &mut resp {
                                    *handoff = Some(summary);
                                }
                                let _ = write_response(&mut stream, &resp);
                            }
                        }
                        Err(err) => {
                            if let Some(mut stream) = reply_stream {
                                let _ = write_response(&mut stream, &IpcResponse::err(err));
                            } else {
                                eprintln!("warning: handoff-to-peer failed: {err}");
                            }
                        }
                    }
                    refresh_ui = true;
                }
                PeerClientEvent::PresentCompleted { req_id, result } => {
                    let reply_stream = self.pending_present_replies.borrow_mut().remove(&req_id);
                    match result {
                        Ok(summary) => {
                            if let Some(mut stream) = reply_stream {
                                let mut resp = IpcResponse::empty_ok();
                                if let IpcResponse::Ok { presented, .. } = &mut resp {
                                    *presented = Some(summary);
                                }
                                let _ = write_response(&mut stream, &resp);
                            }
                        }
                        Err(err) => {
                            if let Some(mut stream) = reply_stream {
                                let _ = write_response(&mut stream, &IpcResponse::err(err));
                            } else {
                                eprintln!("warning: present-to-peer failed: {err}");
                            }
                        }
                    }
                }
                PeerClientEvent::ToolCallCompleted { .. } => {}
            }
        }
        if refresh_ui {
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
        image_id: None,
        placement_id: None,
        sessions: None,
        policy_id: None,
        policies: None,
        pending_policies: None,
        peers: None,
        outbound_peer: None,
        peer_alerts: None,
        session_tree: None,
        handoff: None,
        presented: None,
        visible: None,
    }
}

/// Wall-clock milliseconds for the inventory timestamp.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ssh_config_path() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".ssh").join("config"))
        .unwrap_or_else(|_| PathBuf::from(".ssh/config"))
}

/// Pipes must be nonblocking so a tick never waits on a child.
fn set_nonblocking(fd: i32) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Drains up to the shared per-tick budget. Bytes past `MAX_DISCOVERY_BYTES`
/// are read and discarded rather than left in the pipe: a child that fills the
/// pipe buffer would block forever and `try_wait` would never fire.
fn drain_bounded<R: Read>(reader: &mut R, sink: &mut Vec<u8>, budget: &mut usize) {
    let mut buf = [0u8; 8192];
    while *budget > 0 {
        let want = (*budget).min(buf.len());
        match reader.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => {
                *budget -= n;
                if sink.len() < MAX_DISCOVERY_BYTES {
                    let keep = (MAX_DISCOVERY_BYTES - sink.len()).min(n);
                    sink.extend_from_slice(&buf[..keep]);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// Turns one child's output into sessions or an actionable error. A non-zero
/// exit with no sessions is normal for tmux when no server is running, so it is
/// reported as "no sessions" rather than a hard failure.
fn parse_discovery(
    kind: MuxKind,
    stdout: &str,
    stderr: &str,
    code: Option<i32>,
    host: Option<&str>,
) -> Result<Vec<MuxSession>, String> {
    let sessions = match kind {
        MuxKind::Tmux => mux::parse_tmux(stdout, host),
        MuxKind::Zellij => mux::parse_zellij(stdout, host),
    };
    if !sessions.is_empty() {
        return Ok(sessions);
    }
    if code == Some(0) {
        return Ok(sessions);
    }
    Err(mux::classify_failure(kind.label(), code, stderr))
}
