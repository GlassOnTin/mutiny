//! End-to-end M3.4 tests:
//! 1. Unified hierarchical `session-tree` (`Local Sessions` with `meta` root +
//!    indented organs + `OSC 133;C` busy / `!attention` rollups, `SSH Hosts`,
//!    and `Paired Haven Limb` populated from mock Haven `list_sessions` and
//!    `list_bridges`, including `[agent]` badge on `isAgentRepl == true` and
//!    `chosenSessionName`) + Xvfb GTK4 Session Picker tree widget assertion.
//! 2. `open-session-grid` tiling multiple sessions into a balanced multi-pane
//!    window via IPC (`--sessions` explicit list and auto-selected unattached
//!    local sessions).
//! 3. `handoff-to-peer` against mock Haven:
//!    - Case A: matching `chosenSessionName` already `CONNECTED` in
//!      `list_sessions` -> dispatches `focus_terminal_session` (`0`
//!      `connect_profile` calls).
//!    - Case B: not yet connected -> matches workstation SSH profile from
//!      `list_connections` and dispatches `connect_profile({ profileId,
//!      sessionName })`.
//!    - Case C: bare non-mux shell pane -> returns honest structured error and
//!      posts actionable pane notice without faking handoff.
//! 4. `present-to-peer` routing `.png` -> `present_media`, `.pdf` and
//!    `https://...` -> `present_web`, and `--text` -> `write_clipboard` on mock
//!    Haven, while redacting plaintext clipboard payloads from the audit log.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use mutiny::action::ActionId;
use mutiny::app::AppController;
use mutiny::config::{Config, Overrides, to_toml};
use mutiny::ipc::{IpcRequest, IpcResponse, send_request};
use mutiny::mux::{MuxKind, SessionMode, local_hostname};
use mutiny::peer_client::PeerPollConfig;
use mutiny::peers::{
    InboundNotifyScope, OutboundHavenPeer, PEERS_STORE_VERSION, PeerStore, save as save_peers,
};
use mutiny::session::{SessionGroupKind, SessionNodeKind};
use serde_json::{Value, json};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mutiny-m3-4-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[derive(Debug, Clone)]
struct RecordedToolCall {
    name: String,
    args: Value,
}

struct MockHavenM34Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    sessions: Arc<Mutex<Vec<Value>>>,
    bridges: Arc<Mutex<Vec<Value>>>,
    calls: Arc<Mutex<Vec<RecordedToolCall>>>,
    join: Option<JoinHandle<()>>,
}

impl MockHavenM34Server {
    fn spawn(expected_token: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Haven");
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let bridges = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(Mutex::new(vec![json!({
            "id": "prof-workstation",
            "label": local_hostname(),
            "host": local_hostname(),
            "port": 22,
            "transport": "SSH"
        })]));
        let calls = Arc::new(Mutex::new(Vec::new()));

        let stop_w = Arc::clone(&stop);
        let sessions_w = Arc::clone(&sessions);
        let bridges_w = Arc::clone(&bridges);
        let conns_w = Arc::clone(&connections);
        let calls_w = Arc::clone(&calls);

        let join = thread::spawn(move || {
            while !stop_w.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
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
                            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
                            "tools/call" => {
                                let tool =
                                    parsed["params"]["name"].as_str().unwrap_or("").to_owned();
                                let args = parsed["params"]["arguments"].clone();
                                calls_w.lock().unwrap().push(RecordedToolCall {
                                    name: tool.clone(),
                                    args: args.clone(),
                                });
                                let inner = match tool.as_str() {
                                    "get_app_info" => {
                                        json!({ "packageName": "sh.haven.app", "versionName": "2.5.0" })
                                    }
                                    "get_pending_auth_prompt" | "get_pending_consent" => {
                                        json!({ "pending": false })
                                    }
                                    "list_sessions" => {
                                        let list = sessions_w.lock().unwrap().clone();
                                        json!({ "sessions": list })
                                    }
                                    "list_bridges" => {
                                        let list = bridges_w.lock().unwrap().clone();
                                        json!({ "bridges": list })
                                    }
                                    "list_connections" => {
                                        let list = conns_w.lock().unwrap().clone();
                                        json!({ "connections": list })
                                    }
                                    "focus_terminal_session" => {
                                        json!({ "focused": true, "sessionId": args["sessionId"] })
                                    }
                                    "connect_profile" => json!({
                                        "sessionId": "sess-newly-opened",
                                        "profileId": args["profileId"],
                                        "chosenSessionName": args["sessionName"],
                                        "status": "CONNECTED"
                                    }),
                                    "present_media" => json!({ "presented": "media" }),
                                    "present_web" => json!({ "presented": "web" }),
                                    "write_clipboard" => json!({ "copied": true }),
                                    _ => json!({ "ok": true }),
                                };
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "content": [{ "type": "text", "text": inner.to_string() }],
                                        "isError": false
                                    }
                                })
                            }
                            _ => json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": { "code": -32601, "message": "method not found" }
                            }),
                        };
                        let bytes = serde_json::to_vec(&resp_json).unwrap();
                        let http = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            bytes.len()
                        );
                        let _ = stream.write_all(http.as_bytes());
                        let _ = stream.write_all(&bytes);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            stop,
            sessions,
            bridges,
            calls,
            join: Some(join),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    fn calls_for(&self, name: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.name == name)
            .map(|c| c.args.clone())
            .collect()
    }

    fn clear_action_calls(&self) {
        self.calls.lock().unwrap().retain(|c| {
            !matches!(
                c.name.as_str(),
                "focus_terminal_session"
                    | "connect_profile"
                    | "present_media"
                    | "present_web"
                    | "write_clipboard"
            )
        });
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for MockHavenM34Server {
    fn drop(&mut self) {
        self.stop();
    }
}

fn write_fake_tmux(path: &std::path::Path, script_body: &str) {
    std::fs::write(path, script_body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn send_ipc_async(sock: &std::path::Path, req: IpcRequest) -> JoinHandle<IpcResponse> {
    let sock = sock.to_path_buf();
    thread::spawn(move || send_request(&sock, &req, Duration::from_secs(5)).expect("IPC request"))
}

fn pump_until_finished<T>(ctrl: &Rc<AppController>, handle: JoinHandle<T>, pump_gtk: bool) -> T {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !handle.is_finished() {
        assert!(Instant::now() < deadline, "timed out waiting for IPC reply");
        ctrl.poll_once();
        if pump_gtk {
            while gtk4::glib::MainContext::default().iteration(false) {}
        }
        thread::sleep(Duration::from_millis(8));
    }
    handle.join().expect("join IPC client thread")
}

fn install_paired_haven(dir: &std::path::Path, ctrl: &AppController, url: &str, token: &str) {
    let peers_path = dir.join("peers.json");
    let store = PeerStore {
        version: PEERS_STORE_VERSION,
        inbound_peers: Vec::new(),
        outbound_peers: vec![OutboundHavenPeer {
            id: String::from("hvn-pixel"),
            label: String::from("pixel"),
            url: url.to_owned(),
            bearer_token: token.to_owned(),
            inbound_notify_scope: InboundNotifyScope::HavenOnly,
            paired_at: 1_700_000_000,
            last_connected_at: Some(1_700_000_000),
            signing_secret_b64: None,
            signing_key_id: None,
        }],
    };
    save_peers(&peers_path, &store).unwrap();
    ctrl.set_peers_path(peers_path).unwrap();
    ctrl.set_peer_poll_config(PeerPollConfig {
        connected_interval: Duration::from_millis(80),
        offline_initial_backoff: Duration::from_millis(80),
        offline_max_backoff: Duration::from_millis(200),
        allow_lan: false,
    });
}

#[test]
fn open_session_grid_tiles_multiple_mux_sessions_via_ipc() {
    let dir = temp_dir("grid");
    let fake_tmux = dir.join("fake-tmux");
    // When invoked with `list-sessions`, print 3 unattached sessions + 1 attached session;
    // when invoked to attach (`new-session` / `attach-session`), stay alive like a mux client.
    write_fake_tmux(
        &fake_tmux,
        "#!/bin/sh\n\
         case \"$*\" in\n\
           *list-sessions*)\n\
             printf 'alpha\\t1\\t0\\t1\\t/tmp\\nbeta\\t2\\t0\\t2\\t/tmp\\ngamma\\t1\\t0\\t3\\t/tmp\\nlive\\t1\\t1\\t4\\t/tmp\\n'\n\
             ;;\n\
           *)\n\
             exec /bin/cat\n\
             ;;\n\
         esac\n",
    );

    let mut cfg = Config::default();
    cfg.sessions.tmux_program = fake_tmux.display().to_string();
    cfg.sessions.zellij_program = String::from("/nonexistent-zellij");
    cfg.sessions.ssh_program = String::from("/nonexistent-ssh");
    let cfg_path = dir.join("config.toml");
    std::fs::write(&cfg_path, to_toml(&cfg).unwrap()).unwrap();

    let ctrl = AppController::with_config(
        Some((String::from("/bin/cat"), Vec::new())),
        dir.join("audit.jsonl"),
        cfg_path,
        Overrides::default(),
    );
    let sock = dir.join("mutiny.sock");
    ctrl.start_ipc_listener(&sock).unwrap();
    ctrl.create_window(None).unwrap();

    // 1. Explicit `--sessions s1,s2,s3,s4` opens a balanced 4-pane grid window.
    let h1 = send_ipc_async(
        &sock,
        IpcRequest::OpenSessionGrid {
            sessions: vec![
                String::from("meta"),
                String::from("mutiny"),
                String::from("haven"),
                String::from("radio"),
            ],
            kind: Some(MuxKind::Tmux),
            host: None,
        },
    );
    let resp1 = pump_until_finished(&ctrl, h1, false);
    let grid_win_id = match resp1 {
        IpcResponse::Ok {
            window: Some(w),
            workspace: Some(ws_sum),
            ..
        } => {
            let win_sum = ws_sum.windows.iter().find(|win| win.id == w).unwrap();
            assert_eq!(win_sum.tabs.len(), 1);
            let leaves = win_sum.tabs[0].tree.leaves();
            assert_eq!(leaves.len(), 4, "expected 4 tiled panes in grid window");
            w
        }
        other => panic!("unexpected OpenSessionGrid response: {other:?}"),
    };
    {
        let ws = ctrl.workspace.borrow();
        let tab = ws.window(grid_win_id).unwrap().active_tab_ref().unwrap();
        let attached_names: Vec<String> = tab
            .root
            .leaves()
            .into_iter()
            .map(|pid| ws.pane(pid).unwrap().mux_session().unwrap().name.clone())
            .collect();
        assert_eq!(attached_names, vec!["meta", "mutiny", "haven", "radio"]);
    }

    // 2. Refresh local discovery, then call `open-session-grid` with empty `sessions` ->
    //    auto-tiles the 3 unattached local tmux sessions (`alpha`, `beta`, `gamma`).
    let h_list = send_ipc_async(
        &sock,
        IpcRequest::ListSessions {
            host: None,
            refresh: true,
            unattached_only: false,
        },
    );
    let _ = pump_until_finished(&ctrl, h_list, false);

    let h2 = send_ipc_async(
        &sock,
        IpcRequest::OpenSessionGrid {
            sessions: Vec::new(),
            kind: None,
            host: None,
        },
    );
    let resp2 = pump_until_finished(&ctrl, h2, false);
    match resp2 {
        IpcResponse::Ok {
            window: Some(w2), ..
        } => {
            let ws = ctrl.workspace.borrow();
            let tab = ws.window(w2).unwrap().active_tab_ref().unwrap();
            let auto_names: Vec<String> = tab
                .root
                .leaves()
                .into_iter()
                .map(|pid| ws.pane(pid).unwrap().mux_session().unwrap().name.clone())
                .collect();
            assert_eq!(auto_names, vec!["alpha", "beta", "gamma"]);
        }
        other => panic!("unexpected default OpenSessionGrid response: {other:?}"),
    }

    ctrl.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn handoff_to_peer_focuses_live_session_connects_missing_and_rejects_bare_shell() {
    let dir = temp_dir("handoff");
    let fake_tmux = dir.join("fake-tmux");
    write_fake_tmux(&fake_tmux, "#!/bin/sh\nexec /bin/cat\n");

    let mut cfg = Config::default();
    cfg.sessions.tmux_program = fake_tmux.display().to_string();
    let cfg_path = dir.join("config.toml");
    std::fs::write(&cfg_path, to_toml(&cfg).unwrap()).unwrap();

    let mut mock = MockHavenM34Server::spawn("tok-m34-handoff");
    *mock.sessions.lock().unwrap() = vec![json!({
        "sessionId": "haven-sess-live",
        "profileId": "prof-workstation",
        "label": "Workstation",
        "status": "CONNECTED",
        "transport": "SSH",
        "isAgentRepl": false,
        "sessionManager": "tmux",
        "chosenSessionName": "mutiny"
    })];

    let ctrl = AppController::with_config(
        Some((String::from("/bin/cat"), Vec::new())),
        dir.join("audit.jsonl"),
        cfg_path,
        Overrides::default(),
    );
    let sock = dir.join("mutiny.sock");
    ctrl.start_ipc_listener(&sock).unwrap();
    install_paired_haven(&dir, &ctrl, &mock.url(), "tok-m34-handoff");

    // Wait for initial Haven liveness probe to mark `pixel` Connected.
    let wait_online = Instant::now() + Duration::from_secs(3);
    while Instant::now() < wait_online {
        ctrl.poll_once();
        if !ctrl.list_peers().outbound.is_empty()
            && matches!(
                ctrl.list_peers().outbound[0].status,
                mutiny::peers::OutboundLimbStatus::Connected { .. }
            )
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Bare shell pane created initially:
    let (win_id, _bare_tab, bare_pane) = ctrl.create_window(None).unwrap();

    // Mux-backed pane A (`chosenSessionName == "mutiny"` already live on Haven):
    let (_, _, live_pane) = ctrl
        .open_session(
            MuxKind::Tmux,
            "mutiny",
            None,
            SessionMode::Tab,
            Some(win_id),
            None,
        )
        .unwrap();

    // Mux-backed pane B (`"radio-ops"` not yet open on Haven; detected from pane title `tmux attach -t radio-ops`):
    let (_, _, title_mux_pane) = ctrl.create_tab(Some(win_id), None).unwrap();
    ctrl.workspace
        .borrow_mut()
        .pane_mut(title_mux_pane)
        .unwrap()
        .set_title(String::from("tmux attach -t radio-ops"));

    mock.clear_action_calls();

    // Case A: `mutiny` is already `CONNECTED` on Haven -> `focus_terminal_session` (`0` `connect_profile` calls).
    let h_a = send_ipc_async(
        &sock,
        IpcRequest::HandoffToPeer {
            pane: Some(live_pane),
            peer: None,
            profile_id: None,
        },
    );
    match pump_until_finished(&ctrl, h_a, false) {
        IpcResponse::Ok {
            handoff: Some(summary),
            ..
        } => {
            assert_eq!(summary.action, "focused");
            assert_eq!(summary.session_name, "mutiny");
            assert_eq!(summary.haven_session_id.as_deref(), Some("haven-sess-live"));
        }
        other => panic!("expected Case A focused handoff, got {other:?}"),
    }
    assert_eq!(
        mock.calls_for("focus_terminal_session"),
        vec![json!({ "sessionId": "haven-sess-live" })]
    );
    assert!(
        mock.calls_for("connect_profile").is_empty(),
        "Case A must not call connect_profile"
    );

    mock.clear_action_calls();

    // Case B: `radio-ops` is not yet open on Haven -> matches `prof-workstation` from `list_connections`
    // and dispatches `connect_profile({ "profileId": "prof-workstation", "sessionName": "radio-ops" })`.
    let h_b = send_ipc_async(
        &sock,
        IpcRequest::HandoffToPeer {
            pane: Some(title_mux_pane),
            peer: Some(String::from("pixel")),
            profile_id: None,
        },
    );
    match pump_until_finished(&ctrl, h_b, false) {
        IpcResponse::Ok {
            handoff: Some(summary),
            ..
        } => {
            assert_eq!(summary.action, "connected");
            assert_eq!(summary.session_name, "radio-ops");
            assert_eq!(summary.profile_id.as_deref(), Some("prof-workstation"));
        }
        other => panic!("expected Case B connected handoff, got {other:?}"),
    }
    assert_eq!(
        mock.calls_for("connect_profile"),
        vec![json!({
            "profileId": "prof-workstation",
            "sessionName": "radio-ops"
        })]
    );
    assert!(mock.calls_for("focus_terminal_session").is_empty());

    mock.clear_action_calls();

    // Case C: bare non-mux shell pane -> honest structured error + actionable pane notice, zero Haven calls.
    let h_c = send_ipc_async(
        &sock,
        IpcRequest::HandoffToPeer {
            pane: Some(bare_pane),
            peer: None,
            profile_id: None,
        },
    );
    match pump_until_finished(&ctrl, h_c, false) {
        IpcResponse::Error { message } => {
            assert!(
                message.contains("bare non-mux shell") && message.contains("tmux"),
                "unexpected Case C error: {message}"
            );
        }
        other => panic!("expected Case C error on bare shell, got {other:?}"),
    }
    let notice = ctrl
        .workspace
        .borrow()
        .pane(bare_pane)
        .and_then(|e| e.agent_notice().map(str::to_owned))
        .expect("bare shell pane must receive actionable notice");
    assert!(notice.contains("tmux") && notice.contains("Ctrl+Shift+S"));
    assert!(mock.calls_for("focus_terminal_session").is_empty());
    assert!(mock.calls_for("connect_profile").is_empty());

    ctrl.shutdown_ipc();
    mock.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn present_to_peer_routes_media_web_and_redacted_clipboard_to_haven() {
    let dir = temp_dir("present");
    let cfg_path = dir.join("config.toml");
    std::fs::write(&cfg_path, to_toml(&Config::default()).unwrap()).unwrap();

    let mut mock = MockHavenM34Server::spawn("tok-m34-present");
    let ctrl = AppController::with_config(
        Some((String::from("/bin/cat"), Vec::new())),
        dir.join("audit.jsonl"),
        cfg_path,
        Overrides::default(),
    );
    let sock = dir.join("mutiny.sock");
    ctrl.start_ipc_listener(&sock).unwrap();
    install_paired_haven(&dir, &ctrl, &mock.url(), "tok-m34-present");

    let wait_online = Instant::now() + Duration::from_secs(3);
    while Instant::now() < wait_online {
        ctrl.poll_once();
        if !ctrl.list_peers().outbound.is_empty()
            && matches!(
                ctrl.list_peers().outbound[0].status,
                mutiny::peers::OutboundLimbStatus::Connected { .. }
            )
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let png_path = dir.join("spectrum.png");
    let pdf_path = dir.join("manual.pdf");

    // 1. `.png` path -> `present_media` with auto-matched `profileId: "prof-workstation"` and `mimeType: "image/png"`.
    let h_png = send_ipc_async(
        &sock,
        IpcRequest::PresentToPeer {
            peer: None,
            url: None,
            path: Some(png_path.clone()),
            text: None,
            clipboard: false,
            profile_id: None,
            caption: Some(String::from("Waterfall")),
        },
    );
    match pump_until_finished(&ctrl, h_png, false) {
        IpcResponse::Ok {
            presented: Some(summary),
            ..
        } => {
            assert_eq!(summary.tool, "present_media");
            assert_eq!(summary.profile_id.as_deref(), Some("prof-workstation"));
        }
        other => panic!("expected present_media response, got {other:?}"),
    }
    assert_eq!(
        mock.calls_for("present_media"),
        vec![json!({
            "profileId": "prof-workstation",
            "path": png_path.display().to_string(),
            "mimeType": "image/png",
            "caption": "Waterfall"
        })]
    );

    // 2. `.pdf` path and `https://...` URL -> `present_web`.
    let h_pdf = send_ipc_async(
        &sock,
        IpcRequest::PresentToPeer {
            peer: None,
            url: None,
            path: Some(pdf_path.clone()),
            text: None,
            clipboard: false,
            profile_id: None,
            caption: None,
        },
    );
    match pump_until_finished(&ctrl, h_pdf, false) {
        IpcResponse::Ok {
            presented: Some(summary),
            ..
        } => assert_eq!(summary.tool, "present_web"),
        other => panic!("expected present_web for PDF, got {other:?}"),
    }

    let h_url = send_ipc_async(
        &sock,
        IpcRequest::PresentToPeer {
            peer: None,
            url: Some(String::from("https://haven.sh/docs")),
            path: None,
            text: None,
            clipboard: false,
            profile_id: None,
            caption: Some(String::from("Docs")),
        },
    );
    match pump_until_finished(&ctrl, h_url, false) {
        IpcResponse::Ok {
            presented: Some(summary),
            ..
        } => assert_eq!(summary.tool, "present_web"),
        other => panic!("expected present_web for URL, got {other:?}"),
    }
    assert_eq!(
        mock.calls_for("present_web"),
        vec![
            json!({
                "profileId": "prof-workstation",
                "path": pdf_path.display().to_string()
            }),
            json!({
                "url": "https://haven.sh/docs",
                "caption": "Docs"
            })
        ]
    );

    // 3. `--text` -> `write_clipboard` on Haven, with plaintext strictly redacted in `audit.jsonl`.
    let secret_clip = "top-secret-clipboard-material-99";
    let h_clip = send_ipc_async(
        &sock,
        IpcRequest::PresentToPeer {
            peer: None,
            url: None,
            path: None,
            text: Some(String::from(secret_clip)),
            clipboard: false,
            profile_id: None,
            caption: None,
        },
    );
    match pump_until_finished(&ctrl, h_clip, false) {
        IpcResponse::Ok {
            presented: Some(summary),
            ..
        } => assert_eq!(summary.tool, "write_clipboard"),
        other => panic!("expected write_clipboard response, got {other:?}"),
    }
    assert_eq!(
        mock.calls_for("write_clipboard"),
        vec![json!({ "text": secret_clip })]
    );

    let audit_raw = std::fs::read_to_string(dir.join("audit.jsonl")).unwrap();
    assert!(
        !audit_raw.contains(secret_clip),
        "audit log must never record plaintext clipboard content"
    );
    assert!(
        audit_raw.contains(&format!("text_bytes={}", secret_clip.len())),
        "audit log must record redacted byte length: {audit_raw}"
    );

    ctrl.shutdown_ipc();
    mock.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_tree_groups_local_constellation_ssh_hosts_and_haven_limb_under_xvfb() {
    if gtk4::init().is_err() {
        eprintln!("skipping: no display (CI runs under Xvfb)");
        return;
    }

    let dir = temp_dir("tree-xvfb");
    let fake_tmux = dir.join("fake-tmux");
    write_fake_tmux(
        &fake_tmux,
        "#!/bin/sh\n\
         case \"$*\" in\n\
           *list-sessions*)\n\
             printf 'meta\\t2\\t1\\t1\\t/home/x\\nmutiny\\t1\\t1\\t2\\t/home/x\\nradio\\t1\\t0\\t3\\t/home/x\\n'\n\
             ;;\n\
           *)\n\
             exec /bin/cat\n\
             ;;\n\
         esac\n",
    );

    let mut cfg = Config::default();
    cfg.sessions.tmux_program = fake_tmux.display().to_string();
    cfg.sessions.zellij_program = String::from("/nonexistent-zellij");
    cfg.sessions.ssh_program = String::from("/nonexistent-ssh");
    cfg.sessions.hosts = vec![String::from("meteor")];
    cfg.sessions.root_session = String::from("meta");
    let cfg_path = dir.join("config.toml");
    std::fs::write(&cfg_path, to_toml(&cfg).unwrap()).unwrap();

    let mut mock = MockHavenM34Server::spawn("tok-m34-tree");
    *mock.sessions.lock().unwrap() = vec![
        json!({
            "sessionId": "hvn-s1",
            "profileId": "p-agent",
            "label": "Agent Shell",
            "status": "CONNECTED",
            "transport": "MOSH",
            "isAgentRepl": true,
            "sessionManager": "tmux",
            "chosenSessionName": "remote-ops"
        }),
        json!({
            "sessionId": "hvn-s2",
            "profileId": "p-rdp",
            "label": "Lab Scope",
            "status": "DISCONNECTED",
            "transport": "RDP",
            "isAgentRepl": false
        }),
    ];
    *mock.bridges.lock().unwrap() = vec![json!({
        "source": "usb:1050:0407",
        "sourceKind": "usb",
        "sink": "tcp:127.0.0.1:3240",
        "transport": "usbip",
        "state": "active"
    })];

    let app = gtk4::Application::new(
        Some("dev.mutiny.m34.tree.test"),
        gtk4::gio::ApplicationFlags::NON_UNIQUE,
    );
    app.register(gtk4::gio::Cancellable::NONE).unwrap();

    let ctrl = AppController::with_config(
        Some((String::from("/bin/cat"), Vec::new())),
        dir.join("audit.jsonl"),
        cfg_path,
        Overrides::default(),
    );
    ctrl.attach_gtk_app(&app);
    let sock = dir.join("mutiny.sock");
    ctrl.start_ipc_listener(&sock).unwrap();
    install_paired_haven(&dir, &ctrl, &mock.url(), "tok-m34-tree");

    let (win_id, _, _) = ctrl.create_window(None).unwrap();
    let (_, _, mutiny_pane) = ctrl
        .open_session(
            MuxKind::Tmux,
            "mutiny",
            None,
            SessionMode::Tab,
            Some(win_id),
            None,
        )
        .unwrap();
    // Simulate OSC 133;C (busy foreground command) + OSC 777 notification (!attention) on `mutiny`.
    ctrl.workspace
        .borrow_mut()
        .pane_mut(mutiny_pane)
        .unwrap()
        .advance_bytes(b"\x1b]133;C\x07\x1b]777;notify;Tests;Running\x07");

    // Wait for Haven liveness poll to connect and populate limb inventory.
    let wait_online = Instant::now() + Duration::from_secs(3);
    while Instant::now() < wait_online {
        ctrl.poll_once();
        while gtk4::glib::MainContext::default().iteration(false) {}
        if !ctrl.list_peers().outbound.is_empty()
            && matches!(
                ctrl.list_peers().outbound[0].status,
                mutiny::peers::OutboundLimbStatus::Connected { .. }
            )
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    // 1. Query `session-tree --refresh` over IPC and assert hierarchical groups & rollups.
    let h_tree = send_ipc_async(&sock, IpcRequest::SessionTree { refresh: true });
    let tree = match pump_until_finished(&ctrl, h_tree, true) {
        IpcResponse::Ok {
            session_tree: Some(t),
            ..
        } => t,
        other => panic!("expected session_tree response, got {other:?}"),
    };
    assert_eq!(tree.groups.len(), 3);
    assert_eq!(tree.groups[0].kind, SessionGroupKind::LocalSessions);
    assert_eq!(tree.groups[0].rollup.total_count, 3);
    assert_eq!(tree.groups[0].rollup.busy_count, 1);
    assert_eq!(tree.groups[0].rollup.attention_count, 1);
    assert_eq!(tree.groups[0].items[0].name, "meta");
    assert_eq!(tree.groups[0].items[0].depth, 0);
    assert_eq!(tree.groups[0].items[1].name, "mutiny");
    assert_eq!(tree.groups[0].items[1].depth, 1);
    assert_eq!(tree.groups[0].items[2].name, "radio");
    assert_eq!(tree.groups[0].items[2].depth, 1);

    assert_eq!(tree.groups[1].kind, SessionGroupKind::SshHosts);
    assert!(tree.groups[1].items.iter().any(|i| i.name == "meteor"));

    assert_eq!(tree.groups[2].kind, SessionGroupKind::HavenLimb);
    assert_eq!(tree.groups[2].title, "● haven:pixel");
    assert_eq!(tree.groups[2].rollup.total_count, 3);
    assert_eq!(tree.groups[2].rollup.active_count, 2);
    assert!(
        tree.groups[2]
            .items
            .iter()
            .any(|i| i.node_kind == SessionNodeKind::HavenSession
                && i.is_agent_repl
                && i.detail.contains("[agent]")
                && i.detail.contains("tmux:remote-ops"))
    );
    assert!(
        tree.groups[2]
            .items
            .iter()
            .any(|i| i.node_kind == SessionNodeKind::HavenBridge && i.detail.contains("usbip"))
    );

    // 2. Open the GTK4 Session Picker (`Ctrl+Shift+S` action) under Xvfb and verify rendered
    //    group headers + indented child items.
    let ui_win = ctrl.ui_window(win_id).expect("ui window");
    ctrl.execute_action(ActionId::SessionPicker, Some(win_id));
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        ctrl.poll_once();
        while gtk4::glib::MainContext::default().iteration(false) {}
        if !ctrl.discovery_in_flight() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    for _ in 0..5 {
        ctrl.poll_once();
        while gtk4::glib::MainContext::default().iteration(false) {}
    }

    let headers = ui_win.session_picker_group_headers();
    assert!(
        headers.iter().any(|h| {
            h.starts_with("Local Sessions (") && h.contains("1 busy") && h.contains("!attention:1")
        }),
        "expected Local Sessions rollup header with busy + !attention:1, got {headers:?}"
    );
    assert!(
        headers.iter().any(|h| h.starts_with("SSH Hosts (")),
        "expected SSH Hosts header, got {headers:?}"
    );
    assert!(
        headers
            .iter()
            .any(|h| h.starts_with("● haven:pixel (2/3 active")),
        "expected ● haven:pixel rollup header, got {headers:?}"
    );

    let item_labels = ui_win.session_picker_item_labels();
    assert!(
        item_labels.iter().any(|l| l.starts_with("  ↳ mutiny |")),
        "expected indented organ row under meta root, got {item_labels:?}"
    );
    assert!(
        item_labels
            .iter()
            .any(|l| l.contains("Agent Shell") && l.contains("[agent]")),
        "expected Haven agent session row in picker, got {item_labels:?}"
    );
    assert_eq!(ui_win.session_picker_selected().as_deref(), Some("meta"));

    ui_win.close_programmatically();
    for _ in 0..10 {
        ctrl.poll_once();
        while gtk4::glib::MainContext::default().iteration(false) {}
    }
    ctrl.shutdown_ipc();
    mock.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
