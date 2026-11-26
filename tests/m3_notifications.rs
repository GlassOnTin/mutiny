//! End-to-end M3.3 tests:
//! 1. Unfocused `OSC 133;C` -> `OSC 133;D;<exit>` long-command detector
//!    (`long_command_secs`, focus gating, `priority` `"default"` vs `"high"`,
//!    `!notify` / `!exit` pane badges, and zero scrollback leakage).
//! 2. `OSC 777;notify;Title;Body` focus-aware outbound routing (`unfocused` vs
//!    `always` vs `off`) + per-peer token-bucket rate limiting (`1.0/s`, burst
//!    `5`, `rate_limited_count` in `list-peers`).
//! 3. Default `haven_only` (`inbound_shade_allowlist = []`) zero-`EVERY_CALL`
//!    invariant (`0` calls to `list_notifications` on mock Haven) while still
//!    lighting `!haven` on inbound `CarrierRole::Peer` `present_notice`,
//!    stalled `get_pending_auth_prompt`, and limb disconnect.
//! 4. Opt-in `inbound_shade_allowlist` polling `list_notifications` (`sinceMs`
//!    cursor advancement, client-side package substring filtering, `removed:
//!    true` dismissal sync, and `-32603` listener backoff + `shade_status`) +
//!    GTK4 `!haven` Limb Status Pill badge assertion under Xvfb.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mutiny::action::ActionId;
use mutiny::app::AppController;
use mutiny::config::{Config, Overrides, to_toml};
use mutiny::engine::AttentionReason;
use mutiny::http_mcp;
use mutiny::ipc::{ConsentMode, IpcRequest, IpcResponse, send_request};
use mutiny::notify::DesktopNotifier;
use mutiny::peer_client::PeerPollConfig;
use mutiny::peers::{
    InboundNotifyScope, OutboundHavenPeer, OutboundLimbStatus, PEERS_STORE_VERSION, PeerAlertKind,
    PeerStore,
};
use serde_json::{Value, json};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mutiny-m3-3-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[derive(Default)]
struct RecordingDesktopNotifier {
    sent: RefCell<Vec<(String, String, String)>>,
}

impl DesktopNotifier for RecordingDesktopNotifier {
    fn notify(&self, app_name: &str, title: &str, body: &str) {
        self.sent
            .borrow_mut()
            .push((app_name.to_owned(), title.to_owned(), body.to_owned()));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RaisedNotification {
    title: String,
    body: String,
    priority: String,
    ongoing: bool,
}

#[derive(Clone)]
enum ShadeResponderMode {
    Items(Vec<Value>),
    ListenerDisconnectedError,
}

struct MockHavenNotifyServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    auth_prompt_pending: Arc<AtomicBool>,
    raised: Arc<Mutex<Vec<RaisedNotification>>>,
    list_notifications_calls: Arc<AtomicUsize>,
    list_notifications_since_ms: Arc<Mutex<Vec<u64>>>,
    shade_mode: Arc<Mutex<ShadeResponderMode>>,
    join: Option<JoinHandle<()>>,
}

impl MockHavenNotifyServer {
    fn spawn(expected_token: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Haven");
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let healthy = Arc::new(AtomicBool::new(true));
        let auth_prompt_pending = Arc::new(AtomicBool::new(false));
        let raised = Arc::new(Mutex::new(Vec::new()));
        let list_notifications_calls = Arc::new(AtomicUsize::new(0));
        let list_notifications_since_ms = Arc::new(Mutex::new(Vec::new()));
        let shade_mode = Arc::new(Mutex::new(ShadeResponderMode::Items(Vec::new())));

        let stop_w = Arc::clone(&stop);
        let healthy_w = Arc::clone(&healthy);
        let auth_w = Arc::clone(&auth_prompt_pending);
        let raised_w = Arc::clone(&raised);
        let list_calls_w = Arc::clone(&list_notifications_calls);
        let since_w = Arc::clone(&list_notifications_since_ms);
        let shade_w = Arc::clone(&shade_mode);

        let join = thread::spawn(move || {
            while !stop_w.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if !healthy_w.load(Ordering::Relaxed) {
                            drop(stream);
                            continue;
                        }
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                        let mut reader = BufReader::new(match stream.try_clone() {
                            Ok(s) => s,
                            Err(_) => continue,
                        });
                        let mut req_line = String::new();
                        if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                            continue;
                        }
                        let mut content_len = 0usize;
                        let mut auth_hdr = None::<String>;
                        loop {
                            let mut hdr = String::new();
                            if reader.read_line(&mut hdr).unwrap_or(0) == 0 || hdr == "\r\n" {
                                break;
                            }
                            if let Some((k, v)) = hdr.split_once(':') {
                                if k.trim().eq_ignore_ascii_case("content-length") {
                                    content_len = v.trim().parse().unwrap_or(0);
                                } else if k.trim().eq_ignore_ascii_case("authorization") {
                                    auth_hdr = Some(v.trim().to_owned());
                                }
                            }
                        }
                        let mut body = vec![0u8; content_len];
                        if reader.read_exact(&mut body).is_err() {
                            continue;
                        }
                        let want_auth = format!("Bearer {expected_token}");
                        if auth_hdr.as_deref() != Some(want_auth.as_str()) {
                            let err_txt = r#"{"error":"unauthorized"}"#;
                            let http = format!(
                                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{err_txt}",
                                err_txt.len()
                            );
                            let _ = stream.write_all(http.as_bytes());
                            continue;
                        }
                        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        let id = parsed.get("id").cloned().unwrap_or(json!(1));
                        let method = parsed.get("method").and_then(Value::as_str).unwrap_or("");
                        let resp_json = match method {
                            "initialize" => json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "haven", "version": "2.5.0" }
                                }
                            }),
                            "ping" => json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {}
                            }),
                            "tools/call" => {
                                let tool = parsed["params"]["name"].as_str().unwrap_or("");
                                let args = &parsed["params"]["arguments"];
                                match tool {
                                    "get_app_info" => {
                                        let inner = json!({
                                            "packageName": "sh.haven.app",
                                            "versionName": "2.5.0"
                                        })
                                        .to_string();
                                        json!({
                                            "jsonrpc": "2.0",
                                            "id": id,
                                            "result": {
                                                "content": [{ "type": "text", "text": inner }],
                                                "isError": false
                                            }
                                        })
                                    }
                                    "raise_notification" => {
                                        let rec = RaisedNotification {
                                            title: args["title"].as_str().unwrap_or("").to_owned(),
                                            body: args["body"].as_str().unwrap_or("").to_owned(),
                                            priority: args["priority"]
                                                .as_str()
                                                .unwrap_or("default")
                                                .to_owned(),
                                            ongoing: args["ongoing"].as_bool().unwrap_or(false),
                                        };
                                        raised_w.lock().unwrap().push(rec);
                                        let inner =
                                            json!({"posted": true, "id": 101, "channel": "agent"})
                                                .to_string();
                                        json!({
                                            "jsonrpc": "2.0",
                                            "id": id,
                                            "result": {
                                                "content": [{ "type": "text", "text": inner }],
                                                "isError": false
                                            }
                                        })
                                    }
                                    "get_pending_auth_prompt" => {
                                        let inner = if auth_w.load(Ordering::Relaxed) {
                                            json!({
                                                "pending": true,
                                                "profileId": "prod-bastion",
                                                "label": "ian@bastion",
                                                "requiresPassphrase": true
                                            })
                                        } else {
                                            json!({ "pending": false })
                                        }
                                        .to_string();
                                        json!({
                                            "jsonrpc": "2.0",
                                            "id": id,
                                            "result": {
                                                "content": [{ "type": "text", "text": inner }],
                                                "isError": false
                                            }
                                        })
                                    }
                                    "get_pending_consent" => {
                                        let inner =
                                            json!({ "pending": false, "count": 0, "requests": [] })
                                                .to_string();
                                        json!({
                                            "jsonrpc": "2.0",
                                            "id": id,
                                            "result": {
                                                "content": [{ "type": "text", "text": inner }],
                                                "isError": false
                                            }
                                        })
                                    }
                                    "list_notifications" => {
                                        list_calls_w.fetch_add(1, Ordering::SeqCst);
                                        let since = args["sinceMs"].as_u64().unwrap_or(0);
                                        since_w.lock().unwrap().push(since);
                                        let mode = shade_w.lock().unwrap().clone();
                                        match mode {
                                            ShadeResponderMode::Items(items) => {
                                                let matching: Vec<Value> = items
                                                    .into_iter()
                                                    .filter(|it| {
                                                        it["postTimeMs"].as_u64().unwrap_or(0)
                                                            >= since
                                                    })
                                                    .collect();
                                                let inner = json!({
                                                    "connected": true,
                                                    "count": matching.len(),
                                                    "notifications": matching
                                                })
                                                .to_string();
                                                json!({
                                                    "jsonrpc": "2.0",
                                                    "id": id,
                                                    "result": {
                                                        "content": [{ "type": "text", "text": inner }],
                                                        "isError": false
                                                    }
                                                })
                                            }
                                            ShadeResponderMode::ListenerDisconnectedError => {
                                                json!({
                                                    "jsonrpc": "2.0",
                                                    "id": id,
                                                    "error": {
                                                        "code": -32603,
                                                        "message": "Notification listener is not connected"
                                                    }
                                                })
                                            }
                                        }
                                    }
                                    other => json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "error": { "code": -32601, "message": format!("unknown tool {other}") }
                                    }),
                                }
                            }
                            _ => json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": { "code": -32601, "message": "method not found" }
                            }),
                        };
                        let resp_txt = resp_json.to_string();
                        let http = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_txt}",
                            resp_txt.len()
                        );
                        let _ = stream.write_all(http.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            stop,
            healthy,
            auth_prompt_pending,
            raised,
            list_notifications_calls,
            list_notifications_since_ms,
            shade_mode,
            join: Some(join),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for MockHavenNotifyServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn write_test_files(
    dir: &std::path::Path,
    long_command_secs: u64,
    peer_relay_mode: &str,
    inbound_shade_allowlist: Vec<String>,
    haven_addr: SocketAddr,
    token: &str,
) -> (PathBuf, PathBuf) {
    let cfg_path = dir.join("config.toml");
    let peers_path = dir.join("peers.json");
    let mut cfg = Config::default();
    cfg.notifications.long_command_secs = long_command_secs;
    cfg.notifications.peer_relay_mode = peer_relay_mode.to_owned();
    cfg.notifications.inbound_shade_allowlist = inbound_shade_allowlist;
    std::fs::write(&cfg_path, to_toml(&cfg).unwrap()).unwrap();

    let store = PeerStore {
        version: PEERS_STORE_VERSION,
        inbound_peers: Vec::new(),
        outbound_peers: vec![OutboundHavenPeer {
            id: String::from("out-pixel"),
            label: String::from("pixel"),
            url: format!("http://{haven_addr}/mcp"),
            bearer_token: token.to_owned(),
            inbound_notify_scope: InboundNotifyScope::HavenOnly,
            paired_at: 1_700_000_000,
            last_connected_at: None,
            signing_secret_b64: None,
            signing_key_id: None,
        }],
    };
    mutiny::peers::save(&peers_path, &store).unwrap();
    (cfg_path, peers_path)
}

fn wait_until_connected(ctrl: &Rc<AppController>) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        ctrl.poll_once();
        let peers = ctrl.list_peers();
        if peers
            .outbound
            .iter()
            .any(|p| matches!(p.status, OutboundLimbStatus::Connected { .. }))
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for mock Haven limb to become Connected");
}

fn raw_http_post(addr: SocketAddr, token: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect Mutiny HTTP MCP");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, resp_body) = raw.split_once("\r\n\r\n").expect("HTTP CRLF split");
    let status: u16 = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, resp_body.to_owned())
}

#[test]
fn osc133_long_command_detector_respects_threshold_focus_priority_and_never_leaks_scrollback() {
    let dir = temp_dir("osc133-long");
    let mut mock = MockHavenNotifyServer::spawn("hvn_m33_tok_1");
    let (cfg_path, _peers_path) =
        write_test_files(&dir, 1, "unfocused", Vec::new(), mock.addr, "hvn_m33_tok_1");

    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        cfg_path,
        Overrides::default(),
    );
    let recorder = Rc::new(RecordingDesktopNotifier::default());
    ctrl.set_notifier(recorder.clone());
    ctrl.set_peer_poll_config(PeerPollConfig {
        connected_interval: Duration::from_millis(80),
        offline_initial_backoff: Duration::from_millis(80),
        offline_max_backoff: Duration::from_millis(250),
        allow_lan: false,
    });
    let (_w, _t, pid) = ctrl.create_window(None).unwrap();
    wait_until_connected(&ctrl);

    // Warm shell startup output so initial prompt bytes are drained.
    let warm = Instant::now() + Duration::from_millis(250);
    while Instant::now() < warm {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(10));
    }

    // 1. Short command (< 1s) while unfocused: MUST NOT notify desktop or Haven.
    ctrl.set_force_window_focused_for_test(Some(false));
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .write_to_pty(
            b"printf '\\033]133;C\\007SHORT_SECRET_SCROLLBACK_111\\n\\033]133;D;0\\007'\n",
        )
        .unwrap();
    let short_wait = Instant::now() + Duration::from_millis(300);
    while Instant::now() < short_wait {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        recorder.sent.borrow().is_empty(),
        "short command must not trigger desktop notification"
    );
    assert!(
        mock.raised.lock().unwrap().is_empty(),
        "short command must not relay to Haven"
    );
    assert_eq!(ctrl.workspace.borrow().pane(pid).unwrap().attention(), None);

    // 2. Long command (>= 1s) while FOCUSED in active window: MUST NOT notify or relay.
    ctrl.set_force_window_focused_for_test(Some(true));
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .write_to_pty(b"printf '\\033]133;C\\007FOCUSED_SECRET_SCROLLBACK_222\\n'\n")
        .unwrap();
    let mid_wait = Instant::now() + Duration::from_millis(1100);
    while Instant::now() < mid_wait {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(15));
    }
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .write_to_pty(b"printf '\\033]133;D;0\\007'\n")
        .unwrap();
    let post_focused = Instant::now() + Duration::from_millis(300);
    while Instant::now() < post_focused {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        recorder.sent.borrow().is_empty(),
        "focused pane long command must not notify"
    );
    assert!(
        mock.raised.lock().unwrap().is_empty(),
        "focused pane long command must not relay to Haven"
    );
    assert_eq!(ctrl.workspace.borrow().pane(pid).unwrap().attention(), None);

    // 3. Long command (>= 1s) while UNFOCUSED with `exit 0`:
    //    relays `priority: "default"`, marks `!notify`, and NEVER leaks scrollback!
    ctrl.set_force_window_focused_for_test(Some(false));
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .write_to_pty(b"printf '\\033]133;C\\007SUPER_SECRET_GAMMA_TOKEN_987654\\n'\n")
        .unwrap();
    let run_ok = Instant::now() + Duration::from_millis(1100);
    while Instant::now() < run_ok {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(15));
    }
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .write_to_pty(b"printf '\\033]133;D;0\\007'\n")
        .unwrap();

    let wait_relay = Instant::now() + Duration::from_secs(2);
    while Instant::now() < wait_relay {
        ctrl.poll_once();
        if !mock.raised.lock().unwrap().is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        ctrl.workspace.borrow().pane(pid).unwrap().attention(),
        Some(AttentionReason::OscNotification),
        "exit 0 long command must mark pane urgent for !notify / urgent-next"
    );
    {
        let got = mock.raised.lock().unwrap();
        assert_eq!(got.len(), 1, "expected 1 relayed notification, got {got:?}");
        assert_eq!(got[0].title, "Command finished (exit 0)");
        assert_eq!(got[0].priority, "default");
        assert!(!got[0].ongoing);
        assert!(
            got[0].body.contains(&format!("pane {}", pid.0)),
            "body should identify pane: {}",
            got[0].body
        );
        assert!(
            !got[0].title.contains("SUPER_SECRET")
                && !got[0].body.contains("SUPER_SECRET")
                && !got[0].body.contains("987654"),
            "long-command notification leaked raw scrollback: {:?}",
            got[0]
        );
    }

    // Clear attention before testing non-zero exit.
    ctrl.workspace.borrow_mut().focus_pane(pid).unwrap();
    assert_eq!(ctrl.workspace.borrow().pane(pid).unwrap().attention(), None);

    // 4. Long command (>= 1s) while UNFOCUSED with `exit 7`:
    //    relays `priority: "high"`, marks `CommandFailed(7)` (`!exit 7`), zero leak!
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .write_to_pty(b"printf '\\033]133;C\\007SUPER_SECRET_CRASH_STACK_55555\\n'\n")
        .unwrap();
    let run_fail = Instant::now() + Duration::from_millis(1100);
    while Instant::now() < run_fail {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(15));
    }
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .write_to_pty(b"printf '\\033]133;D;7\\007'\n")
        .unwrap();

    let wait_fail = Instant::now() + Duration::from_secs(2);
    while Instant::now() < wait_fail {
        ctrl.poll_once();
        if mock.raised.lock().unwrap().len() >= 2 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        ctrl.workspace.borrow().pane(pid).unwrap().attention(),
        Some(AttentionReason::CommandFailed(7)),
        "non-zero exit long command must carry CommandFailed(7)"
    );
    {
        let got = mock.raised.lock().unwrap();
        assert_eq!(
            got.len(),
            2,
            "expected 2nd relayed notification, got {got:?}"
        );
        assert_eq!(got[1].title, "Command failed (exit 7)");
        assert_eq!(got[1].priority, "high");
        assert!(
            !got[1].title.contains("SUPER_SECRET")
                && !got[1].body.contains("SUPER_SECRET")
                && !got[1].body.contains("55555"),
            "failed command notification leaked raw scrollback: {:?}",
            got[1]
        );
    }

    ctrl.shutdown_ipc();
    mock.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn osc777_focus_aware_relay_modes_and_token_bucket_rate_limiter() {
    let mut mock = MockHavenNotifyServer::spawn("hvn_m33_tok_2");

    // 1. Mode `always`: relays to Haven even when Mutiny window IS focused.
    {
        let dir = temp_dir("osc777-always");
        let (cfg_path, _) =
            write_test_files(&dir, 20, "always", Vec::new(), mock.addr, "hvn_m33_tok_2");
        let ctrl = AppController::with_config(
            Some((String::from("/bin/sh"), Vec::new())),
            dir.join("audit.jsonl"),
            cfg_path,
            Overrides::default(),
        );
        let rec = Rc::new(RecordingDesktopNotifier::default());
        ctrl.set_notifier(rec.clone());
        ctrl.set_peer_poll_config(PeerPollConfig {
            connected_interval: Duration::from_millis(80),
            offline_initial_backoff: Duration::from_millis(80),
            offline_max_backoff: Duration::from_millis(250),
            allow_lan: false,
        });
        let (_w, _t, pid) = ctrl.create_window(None).unwrap();
        ctrl.set_force_window_focused_for_test(Some(true));
        wait_until_connected(&ctrl);

        ctrl.workspace
            .borrow_mut()
            .pane_mut(pid)
            .unwrap()
            .write_to_pty(b"printf '\\033]777;notify;AlwaysTitle;FocusedBody\\007'\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            ctrl.poll_once();
            if !mock.raised.lock().unwrap().is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(rec.sent.borrow().len(), 1);
        let got = mock.raised.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "AlwaysTitle");
        assert_eq!(got[0].body, "FocusedBody");
        assert_eq!(got[0].priority, "default");
        drop(got);
        ctrl.shutdown_ipc();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 2. Mode `off`: NEVER relays to Haven even when unfocused, while desktop still fires.
    mock.raised.lock().unwrap().clear();
    {
        let dir = temp_dir("osc777-off");
        let (cfg_path, _) =
            write_test_files(&dir, 20, "off", Vec::new(), mock.addr, "hvn_m33_tok_2");
        let ctrl = AppController::with_config(
            Some((String::from("/bin/sh"), Vec::new())),
            dir.join("audit.jsonl"),
            cfg_path,
            Overrides::default(),
        );
        let rec = Rc::new(RecordingDesktopNotifier::default());
        ctrl.set_notifier(rec.clone());
        ctrl.set_peer_poll_config(PeerPollConfig {
            connected_interval: Duration::from_millis(80),
            offline_initial_backoff: Duration::from_millis(80),
            offline_max_backoff: Duration::from_millis(250),
            allow_lan: false,
        });
        let (_w, _t, pid) = ctrl.create_window(None).unwrap();
        ctrl.set_force_window_focused_for_test(Some(false));
        wait_until_connected(&ctrl);

        ctrl.workspace
            .borrow_mut()
            .pane_mut(pid)
            .unwrap()
            .write_to_pty(b"printf '\\033]777;notify;OffTitle;QuietPhone\\007'\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline {
            ctrl.poll_once();
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(rec.sent.borrow().len(), 1, "local desktop still fires");
        assert!(
            mock.raised.lock().unwrap().is_empty(),
            "mode=off must never relay to Haven"
        );
        ctrl.shutdown_ipc();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 3. Mode `unfocused` + TokenBucket burst 5 rate limiting:
    //    - Focused OSC 777 does NOT relay.
    //    - Unfocused burst of 7 immediate OSC 777 events relays 5 and rate-limits 2,
    //      with `rate_limited_count == 2` observable in `list-peers` over IPC.
    mock.raised.lock().unwrap().clear();
    {
        let dir = temp_dir("osc777-burst");
        let op_sock = dir.join("mutiny.sock");
        let (cfg_path, _) = write_test_files(
            &dir,
            20,
            "unfocused",
            Vec::new(),
            mock.addr,
            "hvn_m33_tok_2",
        );
        let ctrl = AppController::with_config(
            Some((String::from("/bin/sh"), Vec::new())),
            dir.join("audit.jsonl"),
            cfg_path,
            Overrides::default(),
        );
        let rec = Rc::new(RecordingDesktopNotifier::default());
        ctrl.set_notifier(rec.clone());
        ctrl.start_ipc_listener(&op_sock).unwrap();
        ctrl.set_peer_poll_config(PeerPollConfig {
            connected_interval: Duration::from_millis(80),
            offline_initial_backoff: Duration::from_millis(80),
            offline_max_backoff: Duration::from_millis(250),
            allow_lan: false,
        });
        let (_w, _t, pid) = ctrl.create_window(None).unwrap();
        wait_until_connected(&ctrl);

        // Focused first: desktop receives 1, Haven receives 0.
        ctrl.set_force_window_focused_for_test(Some(true));
        ctrl.workspace
            .borrow_mut()
            .pane_mut(pid)
            .unwrap()
            .write_to_pty(b"printf '\\033]777;notify;FocusedSkip;NoRelay\\007'\n")
            .unwrap();
        let wait_one = Instant::now() + Duration::from_millis(350);
        while Instant::now() < wait_one {
            ctrl.poll_once();
            if !rec.sent.borrow().is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(rec.sent.borrow().len(), 1);
        assert!(mock.raised.lock().unwrap().is_empty());

        // Now unfocused: emit 7 back-to-back OSC 777 notifications in one printf!
        ctrl.set_force_window_focused_for_test(Some(false));
        let mut burst_cmd = String::from("printf '");
        for i in 1..=7 {
            burst_cmd.push_str(&format!("\\033]777;notify;Burst{i};msg{i}\\007"));
        }
        burst_cmd.push_str("'\n");
        ctrl.workspace
            .borrow_mut()
            .pane_mut(pid)
            .unwrap()
            .write_to_pty(burst_cmd.as_bytes())
            .unwrap();

        let wait_burst = Instant::now() + Duration::from_secs(2);
        while Instant::now() < wait_burst {
            ctrl.poll_once();
            if rec.sent.borrow().len() >= 8 && mock.raised.lock().unwrap().len() >= 5 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        // Give worker thread a moment to flush any queued RPCs so we prove a 6th never arrives.
        thread::sleep(Duration::from_millis(120));
        ctrl.poll_once();

        assert_eq!(
            rec.sent.borrow().len(),
            8,
            "local desktop receives 1 focused + 7 unfocused"
        );
        assert_eq!(
            mock.raised.lock().unwrap().len(),
            5,
            "token bucket burst=5 must cap immediate phone notifications at 5"
        );

        // Verify `rate_limited_count == 2` over the real IPC `ListPeers` verb.
        let op_sock_client = op_sock.clone();
        let ipc_join = thread::spawn(move || {
            send_request(
                &op_sock_client,
                &IpcRequest::ListPeers,
                Duration::from_secs(3),
            )
            .unwrap()
        });
        while !ipc_join.is_finished() {
            ctrl.poll_once();
            thread::sleep(Duration::from_millis(5));
        }
        match ipc_join.join().unwrap() {
            IpcResponse::Ok {
                peers: Some(listing),
                ..
            } => {
                assert_eq!(listing.outbound.len(), 1);
                assert_eq!(
                    listing.outbound[0].rate_limited_count, 2,
                    "6th and 7th immediate events must be counted as rate-limited"
                );
            }
            other => panic!("expected ListPeers Ok, got {other:?}"),
        }

        ctrl.shutdown_ipc();
        let _ = std::fs::remove_dir_all(&dir);
    }

    mock.stop();
}

#[test]
fn default_haven_only_mode_never_polls_list_notifications_and_badges_peer_events() {
    let dir = temp_dir("haven-only");
    let op_sock = dir.join("mutiny.sock");
    let peer_relay_sock = dir.join("peer-haven-mail.sock");
    let mut mock = MockHavenNotifyServer::spawn("hvn_m33_tok_3");
    let (cfg_path, _) = write_test_files(
        &dir,
        20,
        "unfocused",
        Vec::new(), // default haven_only (`inbound_shade_allowlist = []`)
        mock.addr,
        "hvn_m33_tok_3",
    );

    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        cfg_path,
        Overrides::default(),
    );
    ctrl.set_notifier(Rc::new(RecordingDesktopNotifier::default()));
    ctrl.start_ipc_listener(&op_sock).unwrap();
    let inbound_token = "mty_inbound_haven_mail_tok";
    ctrl.start_http_peer_listener(
        &peer_relay_sock,
        inbound_token,
        "haven-mail",
        ConsentMode::Paired,
    )
    .unwrap();
    let mut http = http_mcp::spawn_pairable(
        "127.0.0.1:0".parse().unwrap(),
        ctrl.inbound_peer_table(),
        None,
    )
    .unwrap();
    ctrl.set_peer_poll_config(PeerPollConfig {
        connected_interval: Duration::from_millis(60),
        offline_initial_backoff: Duration::from_millis(60),
        offline_max_backoff: Duration::from_millis(200),
        allow_lan: false,
    });
    let (_w, _t, pid) = ctrl.create_window(None).unwrap();
    ctrl.set_force_window_focused_for_test(Some(true));
    wait_until_connected(&ctrl);

    // 1. Run across multiple active-window liveness ticks: `list_notifications` MUST stay at 0!
    let observe_window = Instant::now() + Duration::from_millis(320);
    while Instant::now() < observe_window {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        mock.list_notifications_calls.load(Ordering::SeqCst),
        0,
        "default haven_only mode must NEVER call list_notifications (EVERY_CALL consent hazard)"
    );
    assert_eq!(
        ctrl.limb_pill_summary().map(|(l, _)| l),
        Some(String::from("● haven:pixel"))
    );

    // 2. Inbound HTTP `CarrierRole::Peer` calls `present_notice`:
    //    tags peer label in both pane notice banner and Haven Alerts inbox, and lights `!haven`!
    let http_addr = http.local_addr;
    let notice_call = thread::spawn(move || {
        raw_http_post(
            http_addr,
            inbound_token,
            &json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": {
                    "name": "present_notice",
                    "arguments": { "message": "VIP deploy mail arrived" }
                }
            })
            .to_string(),
        )
    });
    while !notice_call.is_finished() {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(5));
    }
    let (st, resp_body) = notice_call.join().unwrap();
    assert_eq!(st, 200, "inbound present_notice failed: {resp_body}");

    assert_eq!(
        ctrl.workspace.borrow().pane(pid).unwrap().agent_notice(),
        Some("[haven-mail] VIP deploy mail arrived"),
        "pane notice must be tagged with the inbound peer label"
    );
    assert_eq!(
        ctrl.limb_pill_summary().map(|(l, _)| l),
        Some(String::from("!haven:pixel (1)"))
    );
    assert!(
        mock.raised.lock().unwrap().is_empty(),
        "inbound token-peer present_notice must not echo back via raise_notification"
    );

    // 3. Stalled SSH auth prompt on Haven surfaces `!haven:pixel (2)` alert.
    mock.auth_prompt_pending.store(true, Ordering::Relaxed);
    let wait_auth = Instant::now() + Duration::from_secs(2);
    while Instant::now() < wait_auth {
        ctrl.poll_once();
        if ctrl.unread_peer_alert_count() >= 2 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        ctrl.limb_pill_summary().map(|(l, _)| l),
        Some(String::from("!haven:pixel (2)"))
    );

    // 4. Disconnect mock Haven: surfaces `LimbOffline` alert (`◌ !haven:pixel (2)` after auth clears on offline).
    mock.healthy.store(false, Ordering::Relaxed);
    let wait_off = Instant::now() + Duration::from_secs(2);
    while Instant::now() < wait_off {
        ctrl.poll_once();
        if ctrl
            .list_peer_alerts()
            .iter()
            .any(|a| a.kind == PeerAlertKind::LimbOffline && a.unread)
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let off_pill = ctrl.limb_pill_summary().map(|(l, _)| l).unwrap_or_default();
    assert!(
        off_pill.starts_with("◌ !haven:pixel"),
        "offline limb with unread alerts must badge pill: {off_pill}"
    );

    // Verify `ListPeerAlerts` and `ClearPeerAlerts` over IPC.
    let op_for_list = op_sock.clone();
    let list_join = thread::spawn(move || {
        send_request(
            &op_for_list,
            &IpcRequest::ListPeerAlerts,
            Duration::from_secs(3),
        )
        .unwrap()
    });
    while !list_join.is_finished() {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(5));
    }
    match list_join.join().unwrap() {
        IpcResponse::Ok {
            peer_alerts: Some(alerts),
            ..
        } => {
            assert!(alerts.iter().any(|a| {
                a.kind == PeerAlertKind::PeerNotice
                    && a.peer_label == "haven-mail"
                    && a.body == "VIP deploy mail arrived"
                    && a.unread
            }));
            assert!(
                alerts
                    .iter()
                    .any(|a| a.kind == PeerAlertKind::LimbOffline && a.unread)
            );
        }
        other => panic!("expected ListPeerAlerts Ok, got {other:?}"),
    }

    // Reconnect mock Haven and clear alerts via IPC.
    mock.auth_prompt_pending.store(false, Ordering::Relaxed);
    mock.healthy.store(true, Ordering::Relaxed);
    wait_until_connected(&ctrl);

    let op_for_clear = op_sock.clone();
    let clear_join = thread::spawn(move || {
        send_request(
            &op_for_clear,
            &IpcRequest::ClearPeerAlerts,
            Duration::from_secs(3),
        )
        .unwrap()
    });
    while !clear_join.is_finished() {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(clear_join.join().unwrap(), IpcResponse::Ok { .. }));
    assert_eq!(ctrl.unread_peer_alert_count(), 0);
    assert_eq!(
        ctrl.limb_pill_summary().map(|(l, _)| l),
        Some(String::from("● haven:pixel"))
    );
    assert!(
        ctrl.list_peer_alerts()
            .iter()
            .any(|a| a.kind == PeerAlertKind::LimbReconnected),
        "reconnection transition should be recorded in alert history"
    );
    assert_eq!(
        mock.list_notifications_calls.load(Ordering::SeqCst),
        0,
        "list_notifications count must still be 0 at end of haven_only test"
    );

    http.stop();
    ctrl.shutdown_ipc();
    mock.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn opt_in_shade_allowlist_polls_cursor_filters_syncs_dismissals_backs_off_and_badges_gtk_pill() {
    if gtk4::init().is_err() {
        eprintln!("skipping GTK4 shade badge test: no display (CI runs under Xvfb)");
        return;
    }
    use gtk4::prelude::*;

    let dir = temp_dir("shade-gtk");
    let mut mock = MockHavenNotifyServer::spawn("hvn_m33_tok_4");
    *mock.shade_mode.lock().unwrap() = ShadeResponderMode::Items(vec![
        json!({
            "packageName": "com.whatsapp",
            "appName": "WhatsApp",
            "postTimeMs": 1_700_000_100u64,
            "title": "Alice",
            "text": "Rack power restored",
            "ongoing": false,
            "clearable": true,
            "removed": false
        }),
        json!({
            "packageName": "com.candycrush.noise",
            "appName": "CandyNoise",
            "postTimeMs": 1_700_000_200u64,
            "title": "Promo",
            "text": "Buy extra lives",
            "ongoing": false,
            "clearable": true,
            "removed": false
        }),
        json!({
            "packageName": "sh.haven.app",
            "appName": "Haven",
            "postTimeMs": 1_700_000_300u64,
            "title": "WireGuard",
            "text": "Roaming handshake complete",
            "ongoing": false,
            "clearable": true,
            "removed": false
        }),
    ]);

    let (cfg_path, _) = write_test_files(
        &dir,
        20,
        "unfocused",
        vec![String::from("whatsapp"), String::from("sh.haven.app")],
        mock.addr,
        "hvn_m33_tok_4",
    );

    let app = gtk4::Application::new(
        Some("sh.mutiny.m33.shade.test"),
        gtk4::gio::ApplicationFlags::NON_UNIQUE,
    );
    app.register(gtk4::gio::Cancellable::NONE).unwrap();

    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        cfg_path,
        Overrides::default(),
    );
    ctrl.set_notifier(Rc::new(RecordingDesktopNotifier::default()));
    ctrl.attach_gtk_app(&app);
    ctrl.set_peer_poll_config(PeerPollConfig {
        connected_interval: Duration::from_millis(60),
        offline_initial_backoff: Duration::from_millis(60),
        offline_max_backoff: Duration::from_millis(800),
        allow_lan: false,
    });
    let (win_id, _t, _pid) = ctrl.create_window(None).unwrap();
    while gtk4::glib::MainContext::default().iteration(false) {}
    let ui_win = ctrl.ui_window(win_id).expect("GTK4 MutinyWindow created");

    // Mark window active so opt-in shade polling activates!
    ctrl.set_force_window_focused_for_test(Some(true));
    ctrl.poll_haven_peers_now();

    let wait_poll1 = Instant::now() + Duration::from_secs(3);
    while Instant::now() < wait_poll1 {
        ctrl.poll_once();
        while gtk4::glib::MainContext::default().iteration(false) {}
        if ctrl.unread_peer_alert_count() >= 2 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    // 1. Package allowlist filtering + GTK4 `!haven:pixel (2)` Limb Pill badge:
    assert_eq!(ctrl.unread_peer_alert_count(), 2);
    let alerts1 = ctrl.list_peer_alerts();
    assert!(
        alerts1
            .iter()
            .any(|a| a.package_name.as_deref() == Some("com.whatsapp") && a.title == "Alice"),
        "WhatsApp item should match 'whatsapp' substring: {alerts1:?}"
    );
    assert!(
        alerts1
            .iter()
            .any(|a| a.package_name.as_deref() == Some("sh.haven.app") && a.title == "WireGuard"),
        "Haven item should match 'sh.haven.app' substring: {alerts1:?}"
    );
    assert!(
        !alerts1
            .iter()
            .any(|a| a.package_name.as_deref() == Some("com.candycrush.noise")),
        "non-allowlisted package must be filtered out: {alerts1:?}"
    );
    assert!(ui_win.is_limb_pill_visible());
    assert_eq!(ui_win.limb_pill_label(), "!haven:pixel (2)");

    // Exercising `UrgentNext` when no local pane is urgent opens the Limb Popover inbox cleanly.
    ctrl.execute_action(ActionId::UrgentNext, Some(win_id));
    while gtk4::glib::MainContext::default().iteration(false) {}

    // 2. Cursor advancement (`sinceMs: 1_700_000_301`) + `removed: true` dismissal sync:
    {
        let seen_cursors = mock.list_notifications_since_ms.lock().unwrap().clone();
        assert_eq!(
            seen_cursors.first().copied(),
            Some(0),
            "initial poll must start at sinceMs=0"
        );
    }
    *mock.shade_mode.lock().unwrap() = ShadeResponderMode::Items(vec![json!({
        "packageName": "com.whatsapp",
        "appName": "WhatsApp",
        "postTimeMs": 1_700_000_400u64,
        "title": "Alice",
        "text": "Rack power restored",
        "ongoing": false,
        "clearable": true,
        "removed": true
    })]);
    ctrl.poll_haven_peers_now();

    let wait_dismiss = Instant::now() + Duration::from_secs(3);
    while Instant::now() < wait_dismiss {
        ctrl.poll_once();
        while gtk4::glib::MainContext::default().iteration(false) {}
        if ctrl.unread_peer_alert_count() == 1 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        ctrl.unread_peer_alert_count(),
        1,
        "removed: true record must auto-clear dismissed WhatsApp alert"
    );
    assert_eq!(ui_win.limb_pill_label(), "!haven:pixel (1)");
    {
        let cursors = mock.list_notifications_since_ms.lock().unwrap().clone();
        assert!(
            cursors.contains(&1_700_000_301),
            "cursor must advance to max_post_time_ms + 1 (1700000301), got {cursors:?}"
        );
    }

    // Clear remaining alert via GUI `ActionId::ClearPeerAlerts` -> GTK pill returns to `● haven:pixel`.
    ctrl.execute_action(ActionId::ClearPeerAlerts, Some(win_id));
    while gtk4::glib::MainContext::default().iteration(false) {}
    assert_eq!(ui_win.limb_pill_label(), "● haven:pixel");

    // 3. `-32603` ("Notification listener is not connected") pauses polling with backoff
    //    and populates a calm `shade_status` note on `list_peers`.
    *mock.shade_mode.lock().unwrap() = ShadeResponderMode::ListenerDisconnectedError;
    let calls_before_err = mock.list_notifications_calls.load(Ordering::SeqCst);
    ctrl.poll_haven_peers_now();

    let wait_backoff = Instant::now() + Duration::from_secs(3);
    let mut observed_note = None;
    while Instant::now() < wait_backoff {
        ctrl.poll_once();
        while gtk4::glib::MainContext::default().iteration(false) {}
        if let Some(note) = ctrl
            .list_peers()
            .outbound
            .first()
            .and_then(|p| p.shade_status.clone())
        {
            observed_note = Some(note);
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let note = observed_note.expect("expected shade_status note after -32603 error");
    assert!(
        note.contains("Shade paused") && note.contains("Notification listener is not connected"),
        "unexpected shade_status note: {note}"
    );

    // Let several 60ms liveness ticks elapse (< 800ms backoff): `list_notifications`
    // must NOT be spammed on every tick!
    let calls_after_first_err = mock.list_notifications_calls.load(Ordering::SeqCst);
    assert!(calls_after_first_err > calls_before_err);
    let quiet_until = Instant::now() + Duration::from_millis(260);
    while Instant::now() < quiet_until {
        ctrl.poll_once();
        while gtk4::glib::MainContext::default().iteration(false) {}
        thread::sleep(Duration::from_millis(15));
    }
    assert_eq!(
        mock.list_notifications_calls.load(Ordering::SeqCst),
        calls_after_first_err,
        "shade polling must pause during backoff rather than looping every tick"
    );

    ctrl.close_window(win_id);
    while gtk4::glib::MainContext::default().iteration(false) {}
    ctrl.shutdown_ipc();
    mock.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
