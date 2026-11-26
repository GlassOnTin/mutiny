//! End-to-end M3.1 tests: symmetric inbound `initialize` tap-to-pair, durable
//! `peers.json` invariants, outbound `HavenPeerClient` tap-to-pair + liveness
//! transitions + plaintext URL policy, `PolicyChange` consent enforcement, and
//! GTK4 Pairing Bar + HeaderBar Limb Status Pill under Xvfb.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use mutiny::action::ConsentTier;
use mutiny::app::AppController;
use mutiny::config::Overrides;
use mutiny::crypto;
use mutiny::http_mcp;
use mutiny::ipc::{
    CarrierRole, ConsentMode, IpcRequest, IpcResponse, PeerIdentity, send_request, server_uid,
};
use mutiny::peer_client::PeerPollConfig;
use mutiny::peers::{self, OutboundLimbStatus};
use serde_json::{Value, json};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mutiny-m3-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn raw_http_post(addr: SocketAddr, token: Option<&str>, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect HTTP MCP");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let auth = match token {
        Some(t) => format!("Authorization: Bearer {t}\r\n"),
        None => String::new(),
    };
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
fn inbound_tap_to_pair_approves_mints_sha256_token_rate_limits_429_and_revokes_live() {
    let dir = temp_dir("inbound-pair");
    let op_sock = dir.join("mutiny.sock");
    let ag_sock = dir.join("mutiny-agent.sock");
    let audit_path = dir.join("audit.jsonl");
    let cfg_path = dir.join("config.toml");
    let peers_path = dir.join("peers.json");

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<SocketAddr>();
    let (prompt_tx, prompt_rx) = mpsc::channel::<(u64, String)>();
    let (approve_tx, approve_rx) = mpsc::channel::<(u64, Option<ConsentMode>)>();

    let op_clone = op_sock.clone();
    let ag_clone = ag_sock.clone();
    let server_thread = thread::spawn(move || {
        let ctrl = AppController::with_config(
            Some((String::from("/bin/sh"), Vec::new())),
            audit_path,
            cfg_path,
            Overrides::default(),
        );
        ctrl.set_allow_headless_pairing_for_test(true);
        ctrl.start_ipc_listener(&op_clone).unwrap();
        ctrl.start_agent_listener(&ag_clone, ConsentMode::Ask)
            .unwrap();
        ctrl.create_window(None).unwrap();

        let (gate, rx) = http_mcp::new_pairing_gate();
        ctrl.attach_pairing_receiver(rx);
        let mut http = http_mcp::spawn_pairable(
            "127.0.0.1:0".parse().unwrap(),
            ctrl.inbound_peer_table(),
            Some(gate),
        )
        .unwrap();
        ready_tx.send(http.local_addr).unwrap();

        let mut reported_prompt_id = None;
        while stop_rx.try_recv().is_err() {
            ctrl.poll_once();
            if let Some((pid, cname, _)) = ctrl.pending_pairing_prompt()
                && reported_prompt_id != Some(pid)
            {
                reported_prompt_id = Some(pid);
                let _ = prompt_tx.send((pid, cname));
            }
            if let Ok((pid, decision)) = approve_rx.try_recv() {
                ctrl.resolve_inbound_pairing(pid, decision)
                    .expect("resolve pairing");
            }
            thread::sleep(Duration::from_millis(4));
        }
        http.stop();
        ctrl.shutdown_ipc();
    });

    let http_addr = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("http bound");

    // 1. Non-initialize unauthenticated requests fail immediately with 401.
    let (st_list, _) = raw_http_post(
        http_addr,
        None,
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string(),
    );
    assert_eq!(st_list, 401, "unauthenticated tools/list must fail 401");

    // 2. Start first unauthenticated `initialize` tap-to-pair in background.
    let init_body = json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "clientInfo": { "name": "haven-phone", "version": "1.2.0" }
        }
    })
    .to_string();
    let init_body_2 = init_body.clone();
    let pair_join = thread::spawn(move || raw_http_post(http_addr, None, &init_body));

    let (prompt_id, client_name) = prompt_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("pairing prompt arrived");
    assert_eq!(client_name, "haven-phone");

    // 3. While the first prompt is pending, a second unauthenticated `initialize`
    //    is immediately rejected with HTTP 429 Too Many Requests.
    let (st_busy, busy_body) = raw_http_post(http_addr, None, &init_body_2);
    assert_eq!(
        st_busy, 429,
        "concurrent pairing must be rate-limited: {busy_body}"
    );
    let busy_json: Value = serde_json::from_str(&busy_body).unwrap();
    assert_eq!(busy_json["error"]["code"], json!(-32002));

    // 4. Operator approves the pairing with `Paired` consent mode.
    approve_tx
        .send((prompt_id, Some(ConsentMode::Paired)))
        .unwrap();
    let (st_ok, ok_body) = pair_join.join().unwrap();
    assert_eq!(st_ok, 200, "approved initialize must return 200: {ok_body}");
    let ok_json: Value = serde_json::from_str(&ok_body).unwrap();
    let issued_token = ok_json["result"]["_meta"]["sh.mutiny/pairingToken"]
        .as_str()
        .expect("sh.mutiny/pairingToken issued")
        .to_owned();
    let issued_peer_id = ok_json["result"]["_meta"]["sh.mutiny/peerId"]
        .as_str()
        .expect("sh.mutiny/peerId issued")
        .to_owned();
    assert!(issued_token.starts_with("mty_"));

    // 5. Verify peers.json on disk: mode 0600, stores ONLY SHA-256 hash, zero
    //    plaintext token, and list-peers redacts hashes too.
    let mode = std::fs::metadata(&peers_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let disk_raw = std::fs::read_to_string(&peers_path).unwrap();
    assert!(
        !disk_raw.contains(&issued_token),
        "plaintext inbound token must never hit disk: {disk_raw}"
    );
    assert!(disk_raw.contains(&crypto::sha256_hex(issued_token.as_bytes())));

    let listed = send_request(&op_sock, &IpcRequest::ListPeers, Duration::from_secs(5)).unwrap();
    let listed_str = serde_json::to_string(&listed).unwrap();
    assert!(!listed_str.contains(&issued_token));
    assert!(!listed_str.contains(&crypto::sha256_hex(issued_token.as_bytes())));
    if let IpcResponse::Ok {
        peers: Some(listing),
        ..
    } = listed
    {
        assert_eq!(listing.inbound.len(), 1);
        assert_eq!(listing.inbound[0].id, issued_peer_id);
        assert_eq!(
            listing.inbound[0].consent_mode.carrier_role(),
            CarrierRole::Peer
        );
    } else {
        panic!("expected list-peers Ok");
    }

    // 6. Subsequent authenticated tools/call succeeds immediately and audits as Peer, never Operator.
    let (st_call, call_body) = raw_http_post(
        http_addr,
        Some(&issued_token),
        &json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": { "name": "spawn_pane", "arguments": { "mode": "tab" } }
        })
        .to_string(),
    );
    assert_eq!(st_call, 200, "authenticated call failed: {call_body}");
    let call_val: Value = serde_json::from_str(&call_body).unwrap();
    assert_eq!(call_val["result"]["isError"], json!(false));

    if let IpcResponse::Ok {
        audit: Some(entries),
        ..
    } = send_request(
        &op_sock,
        &IpcRequest::AuditLog { limit: 20 },
        Duration::from_secs(5),
    )
    .unwrap()
    {
        let spawn_entry = entries
            .iter()
            .find(|e| e.verb == "spawn-tab")
            .expect("spawn-tab audited");
        assert_eq!(spawn_entry.carrier, ConsentMode::Paired);
        assert_eq!(spawn_entry.carrier.carrier_role(), CarrierRole::Peer);
        assert!(spawn_entry.peer.is_token_peer());
    } else {
        panic!("audit log query failed");
    }

    // 7. Denial path: an unauthenticated initialize that the operator denies returns 401.
    let init_deny = init_body_2.clone();
    let deny_join = thread::spawn(move || raw_http_post(http_addr, None, &init_deny));
    let (deny_pid, _) = prompt_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second prompt");
    approve_tx.send((deny_pid, None)).unwrap();
    let (st_denied, _) = deny_join.join().unwrap();
    assert_eq!(st_denied, 401, "denied pairing must fail closed with 401");

    // 8. Live revoke-peer takes effect immediately without restarting --http-bind.
    let rev = send_request(
        &op_sock,
        &IpcRequest::RevokePeer {
            id_or_label: issued_peer_id,
        },
        Duration::from_secs(5),
    )
    .unwrap();
    assert!(
        matches!(rev, IpcResponse::Ok { .. }),
        "revoke failed: {rev:?}"
    );

    let (st_after_revoke, _) = raw_http_post(
        http_addr,
        Some(&issued_token),
        &json!({"jsonrpc":"2.0","id":12,"method":"tools/list"}).to_string(),
    );
    assert_eq!(
        st_after_revoke, 401,
        "revoked bearer token must fail 401 immediately"
    );
    let reloaded = peers::load(&peers_path).unwrap();
    assert!(reloaded.inbound_peers.is_empty());

    let _ = stop_tx.send(());
    server_thread.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

struct MockHavenServer {
    addr: SocketAddr,
    healthy: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl MockHavenServer {
    fn spawn(issued_token: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let healthy = Arc::new(AtomicBool::new(true));
        let shutdown = Arc::new(AtomicBool::new(false));
        let healthy_cl = Arc::clone(&healthy);
        let shutdown_cl = Arc::clone(&shutdown);

        let join = thread::spawn(move || {
            while !shutdown_cl.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if !healthy_cl.load(Ordering::Relaxed) {
                            // Simulate tunnel drop / closed socket when offline.
                            drop(stream);
                            continue;
                        }
                        let _ = handle_mock_haven(&mut stream, issued_token);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            healthy,
            shutdown,
            join: Some(join),
        }
    }
}

impl Drop for MockHavenServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn handle_mock_haven(stream: &mut TcpStream, issued_token: &str) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut req_line = String::new();
    reader.read_line(&mut req_line)?;
    let mut content_len = 0usize;
    let mut auth: Option<String> = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some((k, v)) = t.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_len = v.trim().parse().unwrap_or(0),
                "authorization" => auth = Some(v.trim().to_owned()),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; content_len];
    reader.read_exact(&mut body)?;
    let val: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let id = val.get("id").cloned().unwrap_or(Value::Null);
    let method = val.get("method").and_then(Value::as_str).unwrap_or("");

    let (status, resp_body) = match (method, auth.as_deref()) {
        ("initialize", None) => {
            // Delay briefly so the test observes the `Pairing` limb status in flight.
            thread::sleep(Duration::from_millis(120));
            let ok = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "haven", "version": "2.4.0" },
                    "_meta": {
                        "sh.haven/pairingToken": issued_token
                    }
                }
            });
            (200u16, ok.to_string())
        }
        (_, Some(hdr)) if hdr == format!("Bearer {issued_token}") => match method {
            "tools/call" => {
                let tool = val["params"]["name"].as_str().unwrap_or("");
                let payload = match tool {
                    "get_app_info" => json!({"versionName": "2.4.0", "device": "pixel"}),
                    "echo_probe" => json!({"echo": val["params"]["arguments"]}),
                    _ => json!({"ok": true}),
                };
                let ok = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": payload.to_string() }],
                        "isError": false
                    }
                });
                (200, ok.to_string())
            }
            "ping" => (
                200,
                json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string(),
            ),
            _ => (
                200,
                json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string(),
            ),
        },
        _ => (
            401,
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32001,"message":"unauthorized"}})
                .to_string(),
        ),
    };

    let reason = if status == 200 { "OK" } else { "Unauthorized" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
        resp_body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

#[test]
fn outbound_haven_client_tap_to_pair_liveness_transitions_and_public_ip_refusal() {
    let dir = temp_dir("outbound-haven");
    let op_sock = dir.join("mutiny.sock");
    let audit_path = dir.join("audit.jsonl");
    let cfg_path = dir.join("config.toml");
    let peers_path = dir.join("peers.json");
    let mock = MockHavenServer::spawn("hvn_tap_secret_token_99");

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (pill_tx, pill_rx) = mpsc::channel::<Option<String>>();
    let (tool_req_tx, tool_req_rx) = mpsc::channel::<mpsc::Sender<Result<Value, String>>>();
    let op_clone = op_sock.clone();

    let server_thread = thread::spawn(move || {
        let ctrl = AppController::with_config(
            Some((String::from("/bin/sh"), Vec::new())),
            audit_path,
            cfg_path,
            Overrides::default(),
        );
        ctrl.set_peer_poll_config(PeerPollConfig {
            connected_interval: Duration::from_millis(80),
            offline_initial_backoff: Duration::from_millis(50),
            offline_max_backoff: Duration::from_millis(120),
            allow_lan: false,
        });
        ctrl.start_ipc_listener(&op_clone).unwrap();
        ctrl.create_window(None).unwrap();

        let mut last_pill: Option<String> = None;
        while stop_rx.try_recv().is_err() {
            ctrl.poll_once();
            let cur_pill = ctrl.limb_pill_summary().map(|(lbl, _)| lbl);
            if cur_pill != last_pill {
                last_pill = cur_pill.clone();
                let _ = pill_tx.send(cur_pill);
            }
            if let Ok(reply) = tool_req_rx.try_recv() {
                let res = ctrl.call_outbound_haven_tool(
                    "pixel",
                    "echo_probe",
                    json!({"ping": 42}),
                    Duration::from_secs(3),
                );
                let _ = reply.send(res);
            }
            thread::sleep(Duration::from_millis(4));
        }
        ctrl.shutdown_ipc();
    });

    thread::sleep(Duration::from_millis(60));

    // 1. Plaintext HTTP to public internet IPs is refused immediately.
    for public_url in [
        "http://8.8.8.8:8740/mcp",
        "http://1.1.1.1:8740/mcp",
        "http://[2606:4700:4700::1111]:8740/mcp",
    ] {
        let bad = send_request(
            &op_sock,
            &IpcRequest::PairHaven {
                label: String::from("bad"),
                url: String::from(public_url),
                token: Some(String::from("tok")),
                enroll_signer: false,
            },
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(
            matches!(bad, IpcResponse::Error { .. }),
            "public plaintext URL {public_url} must be rejected: {bad:?}"
        );
    }

    // 2. Pair outbound Haven limb with NO pre-shared token (tap-to-pair).
    let url = format!("http://{}/mcp", mock.addr);
    let op_for_pair = op_sock.clone();
    let pair_call = thread::spawn(move || {
        send_request(
            &op_for_pair,
            &IpcRequest::PairHaven {
                label: String::from("pixel"),
                url,
                token: None,
                enroll_signer: false,
            },
            Duration::from_secs(10),
        )
        .unwrap()
    });

    // Observe `◐ pairing…` status transition while handshake is in flight.
    let first_pill = pill_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("pairing pill event");
    assert_eq!(first_pill.as_deref(), Some("◐ pairing…"));

    let pair_resp = pair_call.join().unwrap();
    let paired_id = match pair_resp {
        IpcResponse::Ok {
            outbound_peer: Some(summary),
            ..
        } => {
            assert_eq!(summary.label, "pixel");
            assert!(matches!(
                summary.status,
                OutboundLimbStatus::Connected {
                    ref remote_version,
                    ..
                } if remote_version.as_deref() == Some("2.4.0")
            ));
            summary.id
        }
        other => panic!("expected PairHaven Ok, got {other:?}"),
    };

    // Observe transition to `● haven:pixel`.
    let connected_pill = pill_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("connected pill event");
    assert_eq!(connected_pill.as_deref(), Some("● haven:pixel"));

    // Verify peers.json persisted mode 0600 with bearer_token, while list-peers redacts it.
    let disk_store = peers::load(&peers_path).unwrap();
    assert_eq!(disk_store.outbound_peers.len(), 1);
    assert_eq!(
        disk_store.outbound_peers[0].bearer_token,
        "hvn_tap_secret_token_99"
    );
    let list_resp = send_request(&op_sock, &IpcRequest::ListPeers, Duration::from_secs(5)).unwrap();
    let list_json = serde_json::to_string(&list_resp).unwrap();
    assert!(
        !list_json.contains("hvn_tap_secret_token_99"),
        "list-peers must never leak outbound bearer_token: {list_json}"
    );

    // 3. Generic `call_tool` plumbing invokes `tools/call` on the paired Haven limb.
    let (res_tx, res_rx) = mpsc::channel();
    tool_req_tx.send(res_tx).unwrap();
    let tool_val = res_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("call_tool succeeded");
    let tool_text = tool_val["content"][0]["text"].as_str().unwrap();
    assert!(tool_text.contains("\"ping\":42"), "got {tool_text}");

    // 4. Take mock server offline -> status transitions to offline pill (`◌ !haven:pixel (1)`).
    mock.healthy.store(false, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_offline = false;
    while Instant::now() < deadline {
        if let Ok(Some(lbl)) = pill_rx.recv_timeout(Duration::from_millis(200))
            && lbl.starts_with('◌')
            && lbl.contains("haven:pixel")
        {
            saw_offline = true;
            break;
        }
    }
    assert!(
        saw_offline,
        "expected transition to offline pill `◌ ...haven:pixel...`"
    );

    // 5. Bring mock server back -> recovers to `● haven:pixel`.
    mock.healthy.store(true, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_recovered = false;
    while Instant::now() < deadline {
        if let Ok(Some(lbl)) = pill_rx.recv_timeout(Duration::from_millis(200))
            && lbl == "● haven:pixel"
        {
            saw_recovered = true;
            break;
        }
    }
    assert!(saw_recovered, "expected recovery to `● haven:pixel`");

    // 6. Revoke outbound peer -> pill hides (`None`) and peers.json is emptied.
    assert!(matches!(
        send_request(
            &op_sock,
            &IpcRequest::RevokePeer {
                id_or_label: paired_id
            },
            Duration::from_secs(5)
        )
        .unwrap(),
        IpcResponse::Ok { .. }
    ));
    assert!(peers::load(&peers_path).unwrap().outbound_peers.is_empty());

    let _ = stop_tx.send(());
    server_thread.join().unwrap();
    drop(mock);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn policy_change_verbs_never_bypass_consent_and_paired_peers_never_gain_operator() {
    let dir = temp_dir("policy-guard");
    let op_sock = dir.join("mutiny.sock");
    let ag_sock = dir.join("mutiny-agent.sock");
    let peer_sock = dir.join("mutiny-peer.sock");
    let audit_path = dir.join("audit.jsonl");
    let cfg_path = dir.join("config.toml");

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<SocketAddr>();
    let op_cl = op_sock.clone();
    let ag_cl = ag_sock.clone();
    let peer_cl = peer_sock.clone();

    let server_thread = thread::spawn(move || {
        let ctrl = AppController::with_config(
            Some((String::from("/bin/sh"), Vec::new())),
            audit_path,
            cfg_path,
            Overrides::default(),
        );
        ctrl.start_ipc_listener(&op_cl).unwrap();
        ctrl.start_agent_listener(&ag_cl, ConsentMode::Ask).unwrap();
        // Pre-grant BOTH this process on the agent socket AND the Paired HTTP peer!
        let pid = std::process::id() as i32;
        let proc_peer = PeerIdentity {
            uid: server_uid(),
            pid,
            start_time: mutiny::ipc::process_start_time(pid),
            token_peer: None,
        };
        ctrl.grant_session(proc_peer, "granted-local-agent");

        ctrl.start_http_peer_listener(&peer_cl, "paired-tok-1", "phone", ConsentMode::Paired)
            .unwrap();
        ctrl.grant_session(
            PeerIdentity::for_token("paired-tok-1", server_uid()),
            "phone",
        );
        ctrl.create_window(None).unwrap();

        let http = http_mcp::spawn_pairable(
            "127.0.0.1:0".parse().unwrap(),
            ctrl.inbound_peer_table(),
            None,
        )
        .unwrap();
        ready_tx.send(http.local_addr).unwrap();

        while stop_rx.try_recv().is_err() {
            ctrl.poll_once();
            thread::sleep(Duration::from_millis(4));
        }
        ctrl.shutdown_ipc();
    });

    let http_addr = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();

    // 1. Standing policy proposals naming `pair-haven` or `revoke-peer` are rejected.
    for forbidden in ["pair-haven", "revoke-peer", "list-peers"] {
        let prop = send_request(
            &ag_sock,
            &IpcRequest::ProposePolicy {
                description: format!("cover {forbidden}"),
                verbs: vec![String::from(forbidden)],
                arg_constraints: Default::default(),
                max_calls_per_minute: 10,
                expires_in_minutes: 5,
            },
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(
            matches!(prop, IpcResponse::Error { .. }),
            "{forbidden} must be rejected in standing policy scope: {prop:?}"
        );
    }

    // 2. Ordinary Act verb (`SaveSession`) succeeds on the session-granted `Ask`
    //    agent socket, proving the session grant IS active.
    assert!(matches!(
        send_request(&ag_sock, &IpcRequest::SaveSession, Duration::from_secs(5)).unwrap(),
        IpcResponse::Ok { .. }
    ));

    // 3. Despite holding a session grant, `PairHaven` and `RevokePeer` (`ConsentTier::PolicyChange`)
    //    fail closed on the headless `Ask` agent socket instead of riding the grant.
    for req in [
        IpcRequest::PairHaven {
            label: String::from("rogue"),
            url: String::from("http://127.0.0.1:9999/mcp"),
            token: Some(String::from("tok")),
            enroll_signer: false,
        },
        IpcRequest::RevokePeer {
            id_or_label: String::from("phone"),
        },
    ] {
        assert_eq!(req.consent_tier(), ConsentTier::PolicyChange);
        let resp = send_request(&ag_sock, &req, Duration::from_secs(5)).unwrap();
        assert!(
            matches!(resp, IpcResponse::Error { .. }),
            "session grant must never bypass PolicyChange verb {req:?}: {resp:?}"
        );
    }

    // 4. Even a `Paired` HTTP MCP peer cannot execute `revoke_peer` or `pair_haven`
    //    without explicit per-action GUI confirmation, and never gains Operator.
    for (tool, args) in [
        ("revoke_peer", json!({"id_or_label": "phone"})),
        (
            "pair_haven",
            json!({"label": "rogue", "url": "http://127.0.0.1:9999/mcp", "token": "x"}),
        ),
    ] {
        let (st, body) = raw_http_post(
            http_addr,
            Some("paired-tok-1"),
            &json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "tools/call",
                "params": { "name": tool, "arguments": args }
            })
            .to_string(),
        );
        assert_eq!(st, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["result"]["isError"],
            json!(true),
            "Paired HTTP peer must not auto-approve PolicyChange tool {tool}: {v}"
        );
    }

    // Verify phone peer was NOT self-revoked and still lists cleanly.
    if let IpcResponse::Ok {
        peers: Some(listing),
        ..
    } = send_request(&op_sock, &IpcRequest::ListPeers, Duration::from_secs(5)).unwrap()
    {
        assert_eq!(listing.inbound.len(), 1);
        assert_eq!(listing.inbound[0].label, "phone");
    } else {
        panic!("expected phone peer still present");
    }

    let _ = stop_tx.send(());
    server_thread.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gtk_window_renders_pairing_bar_and_headerbar_limb_status_pill() {
    let has_display = std::env::var("DISPLAY").is_ok_and(|d| !d.is_empty())
        || std::env::var("WAYLAND_DISPLAY").is_ok_and(|d| !d.is_empty());
    if !has_display || gtk4::init().is_err() {
        eprintln!("skipping GTK pairing bar & limb pill widget test: no display");
        return;
    }

    use gtk4::gio::prelude::*;

    let dir = temp_dir("gtk-pairing-ui");
    let app = gtk4::Application::builder()
        .application_id("dev.mutiny.TestM3Pairing")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(gtk4::gio::Cancellable::NONE).unwrap();

    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        Overrides::default(),
    );
    ctrl.attach_gtk_app(&app);
    ctrl.start_agent_listener(&dir.join("mutiny-agent.sock"), ConsentMode::Ask)
        .unwrap();
    let (win_id, _, _) = ctrl.create_window(None).unwrap();

    let (gate, rx) = http_mcp::new_pairing_gate();
    ctrl.attach_pairing_receiver(rx);
    let mut http = http_mcp::spawn_pairable(
        "127.0.0.1:0".parse().unwrap(),
        ctrl.inbound_peer_table(),
        Some(gate),
    )
    .unwrap();
    let http_addr = http.local_addr;

    let pump = |ctrl: &std::rc::Rc<AppController>, rounds: usize| {
        let ctx = gtk4::glib::MainContext::default();
        for _ in 0..rounds {
            ctrl.poll_once();
            while ctx.iteration(false) {}
            thread::sleep(Duration::from_millis(4));
        }
    };

    pump(&ctrl, 10);
    let ui_win = ctrl.ui_window(win_id).expect("GTK MutinyWindow");
    assert!(
        !ui_win.is_limb_pill_visible(),
        "Limb Status Pill must stay hidden when zero peers are paired"
    );
    assert!(
        !ui_win.is_pairing_bar_visible(),
        "Pairing Bar must stay hidden when idle"
    );

    // Trigger inbound tap-to-pair from a background client thread.
    let pair_thread = thread::spawn(move || {
        raw_http_post(
            http_addr,
            None,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "clientInfo": { "name": "haven-pixel" }
                }
            })
            .to_string(),
        )
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !ui_win.is_pairing_bar_visible() {
        pump(&ctrl, 3);
    }
    assert!(
        ui_win.is_pairing_bar_visible(),
        "Pairing Bar must become visible on unauthenticated initialize"
    );
    assert!(ui_win.pairing_bar_text().contains("haven-pixel"));

    // Select ObserveOnly (index 2) and fire the keyboard-accessible window action.
    ui_win.select_pairing_mode_index(2);
    gtk4::prelude::WidgetExt::activate_action(&ui_win.gtk_window(), "win.approve-pairing", None)
        .expect("activate win.approve-pairing");
    pump(&ctrl, 10);

    let (st, body) = pair_thread.join().unwrap();
    assert_eq!(st, 200, "GTK approval must return 200: {body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(
        v["result"]["_meta"]["sh.mutiny/pairingToken"]
            .as_str()
            .unwrap()
            .starts_with("mty_")
    );

    assert!(
        !ui_win.is_pairing_bar_visible(),
        "Pairing Bar must hide once resolved"
    );
    assert!(
        ui_win.is_limb_pill_visible(),
        "Limb Status Pill must appear once a peer is paired"
    );
    assert_eq!(ui_win.limb_pill_label(), "● limb:haven-pixel");

    http.stop();
    ctrl.close_window(win_id);
    ctrl.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}
