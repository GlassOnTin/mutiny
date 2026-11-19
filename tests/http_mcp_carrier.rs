//! The authenticated HTTP MCP carrier: an unpaired client must be refused, and
//! a network client must never be assigned Operator.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use mutiny::app::AppController;
use mutiny::config::Overrides;
use mutiny::http_mcp::{HttpMcpConfig, HttpPeer, spawn};
use mutiny::ipc::{ConsentMode, IpcRequest, IpcResponse, send_request};
use serde_json::{Value, json};

const TOKEN: &str = "pairing-token-0123456789abcdef";

fn post(addr: SocketAddr, token: Option<&str>, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read");
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (status, body)
}

struct Harness {
    http_addr: SocketAddr,
    operator_sock: PathBuf,
    stop: Option<thread::JoinHandle<()>>,
    stop_tx: std::sync::mpsc::Sender<()>,
    _dir: PathBuf,
}

fn harness(tag: &str, agent_mode: ConsentMode) -> Harness {
    harness_peers(tag, agent_mode, &[("http-peer", TOKEN)])
}

/// One relay carrier per peer, so each token maps to its own identity.
fn harness_peers(tag: &str, agent_mode: ConsentMode, spec: &[(&str, &str)]) -> Harness {
    let with_modes: Vec<(&str, &str, ConsentMode)> =
        spec.iter().map(|(l, t)| (*l, *t, agent_mode)).collect();
    harness_peers_with_modes(tag, agent_mode, &with_modes)
}

/// Each peer gets its own consent mode, independent of the agent carrier's.
fn harness_peers_with_modes(
    tag: &str,
    agent_mode: ConsentMode,
    spec: &[(&str, &str, ConsentMode)],
) -> Harness {
    let dir = std::env::temp_dir().join(format!("mutiny-http-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let operator_sock = dir.join("mutiny.sock");
    let agent_sock = dir.join("mutiny-agent.sock");

    let peers: Vec<HttpPeer> = spec
        .iter()
        .enumerate()
        .map(|(i, (label, token, _mode))| HttpPeer {
            label: (*label).to_owned(),
            token: (*token).to_owned(),
            socket: dir.join(format!("mutiny-agent-peer-{i}.sock")),
        })
        .collect();
    let modes: Vec<ConsentMode> = spec.iter().map(|(_, _, m)| *m).collect();

    let handle = spawn(HttpMcpConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        peers: peers.clone(),
    })
    .expect("spawn http");
    let http_addr = handle.local_addr;

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let op = operator_sock.clone();
    let ag = agent_sock.clone();
    let audit = dir.join("audit.jsonl");
    let cfg = dir.join("config.toml");
    let stop = thread::spawn(move || {
        let ctrl = AppController::with_config(
            Some((String::from("/bin/sh"), Vec::new())),
            audit,
            cfg,
            Overrides::default(),
        );
        ctrl.start_ipc_listener(&op).expect("operator listener");
        ctrl.start_agent_listener(&ag, agent_mode)
            .expect("agent listener");
        for (peer, mode) in peers.iter().zip(modes.iter()) {
            ctrl.start_http_peer_listener(&peer.socket, &peer.token, &peer.label, *mode)
                .expect("peer relay listener");
        }
        ctrl.create_window(None).expect("pane");
        while stop_rx.try_recv().is_err() {
            ctrl.poll_once();
            thread::sleep(Duration::from_millis(5));
        }
        ctrl.shutdown_ipc();
        drop(handle);
    });
    thread::sleep(Duration::from_millis(250));
    Harness {
        http_addr,
        operator_sock,
        stop: Some(stop),
        stop_tx,
        _dir: dir,
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(handle) = self.stop.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_dir_all(&self._dir);
    }
}

fn audit_entries(h: &Harness) -> Vec<Value> {
    match send_request(
        &h.operator_sock,
        &IpcRequest::AuditLog { limit: 100 },
        Duration::from_secs(5),
    )
    .expect("audit")
    {
        IpcResponse::Ok {
            audit: Some(entries),
            ..
        } => entries
            .into_iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect(),
        other => panic!("unexpected audit response: {other:?}"),
    }
}

#[test]
fn unpaired_clients_are_refused_and_paired_clients_are_served() {
    let h = harness("unpaired", ConsentMode::Paired);
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","clientInfo":{"name":"haven-phone"}}}).to_string();

    // No token at all.
    let (status, body) = post(h.http_addr, None, &initialize);
    assert_eq!(
        status, 401,
        "no token must be refused, got {status}: {body}"
    );
    assert!(body.contains("unpaired"), "body should say why: {body}");

    // Wrong token.
    let (status, _) = post(h.http_addr, Some("wrong-token"), &initialize);
    assert_eq!(status, 401, "wrong token must be refused");

    // Right token, wrong scheme.
    let (status, _) = post(h.http_addr, Some(TOKEN), &initialize);
    assert_eq!(status, 200, "correct token must be served");

    // A paired client can initialize and call a tool.
    let (status, body) = post(h.http_addr, Some(TOKEN), &initialize);
    assert_eq!(status, 200);
    let reply: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(reply["result"]["serverInfo"]["name"], json!("mutiny"));

    let call = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_workspace","arguments":{}}}).to_string();
    let (status, body) = post(h.http_addr, Some(TOKEN), &call);
    assert_eq!(status, 200);
    let reply: Value = serde_json::from_str(&body).expect("json");
    assert!(
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("windows")
    );

    // Only POST /mcp is served.
    let mut stream = TcpStream::connect(h.http_addr).unwrap();
    stream
        .write_all(b"GET /mcp HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n")
        .unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    assert!(
        resp.starts_with("HTTP/1.1 404"),
        "GET must not be served: {resp}"
    );
}

#[test]
fn network_carrier_never_assigns_operator_even_if_the_client_claims_it() {
    let h = harness("carrier", ConsentMode::Paired);

    // Initialize, then attempt to smuggle a consent mode through the JSON-RPC
    // body. The carrier assigns identity; the body cannot influence it.
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","clientInfo":{"name":"haven-phone"}}}).to_string();
    assert_eq!(post(h.http_addr, Some(TOKEN), &initialize).0, 200);

    let smuggle = json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "consent_mode":"operator",
        "params":{"name":"list_workspace","arguments":{},"consent_mode":"operator"}
    })
    .to_string();
    let (status, body) = post(h.http_addr, Some(TOKEN), &smuggle);
    assert_eq!(status, 200);
    let reply: Value = serde_json::from_str(&body).expect("json");
    assert!(
        reply["error"].is_null(),
        "call should succeed as paired: {body}"
    );

    let entries = audit_entries(&h);
    // This slice is stateless per request: there is no MCP session id, so the
    // name sent in initialize does not persist and every HTTP call is audited
    // under the carrier's own label.
    let http_entries: Vec<&Value> = entries
        .iter()
        .filter(|e| e["client"] == json!("http-peer"))
        .collect();
    assert!(
        !http_entries.is_empty(),
        "HTTP calls must be audited: {entries:?}"
    );
    for entry in &http_entries {
        assert_ne!(
            entry["carrier"],
            json!("operator"),
            "a network client must never be assigned Operator: {entry}"
        );
        assert_eq!(
            entry["carrier"],
            json!("paired"),
            "carrier is the agent mode: {entry}"
        );
    }

    // An unpaired attempt must reach no verb at all. Count only HTTP-originated
    // entries: reading the audit log is itself an audited verb, so the total
    // length grows on every read regardless of what the refused client did.
    let http_count = || {
        audit_entries(&h)
            .iter()
            .filter(|e| e["client"] == json!("http-peer"))
            .count()
    };
    let before = http_count();
    let call = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_workspace","arguments":{}}}).to_string();
    assert_eq!(post(h.http_addr, None, &call).0, 401);
    thread::sleep(Duration::from_millis(150));
    assert_eq!(
        http_count(),
        before,
        "a refused client must not reach the dispatcher"
    );
}

#[test]
fn observe_only_agent_carrier_denies_act_over_http() {
    let h = harness("observe", ConsentMode::ObserveOnly);
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"observer-phone"}}}).to_string();
    assert_eq!(post(h.http_addr, Some(TOKEN), &initialize).0, 200);

    // Reads work.
    let read = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_workspace","arguments":{}}}).to_string();
    let (_, body) = post(h.http_addr, Some(TOKEN), &read);
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["result"]["isError"], json!(false));

    // Writes are denied by the carrier, not by the HTTP layer.
    let write = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"spawn_pane","arguments":{"mode":"split-right"}}}).to_string();
    let (_, body) = post(h.http_addr, Some(TOKEN), &write);
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        reply["result"]["isError"],
        json!(true),
        "observe-only must deny Act: {body}"
    );
    assert!(body.contains("observe-only"));
}

const TOKEN_A: &str = "peer-token-aaaaaaaaaaaaaaaaaaaa";
const TOKEN_B: &str = "peer-token-bbbbbbbbbbbbbbbbbbbb";

/// The rung's Done-when: two peer tokens must be two identities in the durable
/// trail, not one shared mutiny pid.
#[test]
fn two_peer_tokens_produce_two_distinct_audit_identities() {
    let h = harness_peers(
        "two-identities",
        ConsentMode::Paired,
        &[("phone", TOKEN_A), ("laptop", TOKEN_B)],
    );
    let call = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_workspace","arguments":{}}}).to_string();

    assert_eq!(post(h.http_addr, Some(TOKEN_A), &call).0, 200);
    assert_eq!(post(h.http_addr, Some(TOKEN_B), &call).0, 200);
    thread::sleep(Duration::from_millis(200));

    let entries = audit_entries(&h);
    let peer_entries = |label: &str| -> Vec<Value> {
        entries
            .iter()
            .filter(|e| e["client"] == json!(label))
            .cloned()
            .collect()
    };
    let phone = peer_entries("phone");
    let laptop = peer_entries("laptop");
    assert!(
        !phone.is_empty(),
        "phone calls must be audited: {entries:?}"
    );
    assert!(
        !laptop.is_empty(),
        "laptop calls must be audited: {entries:?}"
    );

    // Each peer's identity is the fingerprint of its own token.
    assert_eq!(
        phone[0]["peer"]["token_peer"],
        json!(mutiny::ipc::token_fingerprint(TOKEN_A)),
        "identity must be derived from the presented token: {}",
        phone[0]
    );
    assert_eq!(
        laptop[0]["peer"]["token_peer"],
        json!(mutiny::ipc::token_fingerprint(TOKEN_B))
    );
    assert_ne!(
        phone[0]["peer"]["token_peer"], laptop[0]["peer"]["token_peer"],
        "two tokens must give two distinct audit identities"
    );

    // Stable within a peer, and not the relaying process's pid.
    for entry in &phone {
        assert_eq!(entry["peer"]["token_peer"], phone[0]["peer"]["token_peer"]);
        assert_eq!(
            entry["peer"]["pid"],
            json!(0),
            "a token peer is not a process"
        );
    }
    for entry in phone.iter().chain(laptop.iter()) {
        assert_ne!(
            entry["carrier"],
            json!("operator"),
            "never Operator: {entry}"
        );
        assert_eq!(entry["carrier"], json!("paired"));
    }

    // A token outside the configured set is still refused, and reaches no verb.
    let before = entries.len();
    assert_eq!(
        post(h.http_addr, Some("peer-token-cccccccccccccccccccc"), &call).0,
        401
    );
    thread::sleep(Duration::from_millis(150));
    let after = audit_entries(&h);
    let dispatched = |es: &[Value]| {
        es.iter()
            .filter(|e| e["client"] == json!("phone") || e["client"] == json!("laptop"))
            .count()
    };
    assert_eq!(
        dispatched(&after),
        dispatched(&entries),
        "refused token reached a verb"
    );
    assert!(after.len() >= before);
}

/// Per-peer consent MODE. The agent carrier here is pinned to observe-only, so
/// an implementation that still read the shared `agent_mode` would deny the
/// paired peer too. That is what makes this test discriminate.
#[test]
fn each_peer_carries_its_own_consent_mode_not_the_shared_agent_mode() {
    let h = harness_peers_with_modes(
        "peer-modes",
        ConsentMode::ObserveOnly,
        &[
            ("trusted", TOKEN_A, ConsentMode::Paired),
            ("readonly", TOKEN_B, ConsentMode::ObserveOnly),
        ],
    );
    let read = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_workspace","arguments":{}}}).to_string();
    let act = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"spawn_pane","arguments":{"mode":"split-right"}}}).to_string();

    // Observe passes for both peers.
    for token in [TOKEN_A, TOKEN_B] {
        let (status, body) = post(h.http_addr, Some(token), &read);
        assert_eq!(status, 200);
        let reply: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reply["result"]["isError"], json!(false), "{body}");
    }

    // The paired peer may Act even though the agent carrier is observe-only.
    let (_, body) = post(h.http_addr, Some(TOKEN_A), &act);
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        reply["result"]["isError"],
        json!(false),
        "the paired peer's own mode must govern, not the agent carrier's: {body}"
    );

    // The observe-only peer may not.
    let (_, body) = post(h.http_addr, Some(TOKEN_B), &act);
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["result"]["isError"], json!(true), "{body}");
    assert!(body.contains("observe-only"), "{body}");

    thread::sleep(Duration::from_millis(200));
    let entries = audit_entries(&h);
    let for_label = |label: &str| -> Vec<Value> {
        entries
            .iter()
            .filter(|e| e["client"] == json!(label))
            .cloned()
            .collect()
    };
    let trusted = for_label("trusted");
    let readonly = for_label("readonly");
    assert!(!trusted.is_empty() && !readonly.is_empty(), "{entries:?}");
    for entry in &trusted {
        assert_eq!(entry["carrier"], json!("paired"), "{entry}");
    }
    for entry in &readonly {
        assert_eq!(entry["carrier"], json!("observe-only"), "{entry}");
    }
    assert!(
        readonly.iter().any(|e| e["decision"] == json!("denied")),
        "the denial must be audited: {readonly:?}"
    );
    for entry in trusted.iter().chain(readonly.iter()) {
        assert_ne!(
            entry["carrier"],
            json!("operator"),
            "never Operator: {entry}"
        );
    }

    // Unpaired refusal is unchanged by per-peer modes.
    assert_eq!(post(h.http_addr, None, &read).0, 401);
    assert_eq!(post(h.http_addr, Some("not-a-peer-token"), &read).0, 401);
}

/// A peer pinned to ask still prompts, and a grant for one peer does not cover
/// another. Identity was already per peer; this checks mode and grants compose.
#[test]
fn an_ask_peer_is_not_escalated_by_a_paired_sibling() {
    let h = harness_peers_with_modes(
        "ask-sibling",
        ConsentMode::Paired,
        &[
            ("open", TOKEN_A, ConsentMode::Paired),
            ("guarded", TOKEN_B, ConsentMode::Ask),
        ],
    );
    let act = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"spawn_pane","arguments":{"mode":"split-right"}}}).to_string();

    let (_, body) = post(h.http_addr, Some(TOKEN_A), &act);
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["result"]["isError"], json!(false), "{body}");

    // The ask peer has no GUI window in this harness, so it fails closed rather
    // than inheriting its sibling's paired trust.
    let (_, body) = post(h.http_addr, Some(TOKEN_B), &act);
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["result"]["isError"], json!(true), "{body}");
    assert!(
        body.contains("consent"),
        "expected a consent failure, not a paired allow: {body}"
    );

    thread::sleep(Duration::from_millis(200));
    let entries = audit_entries(&h);
    for entry in entries.iter().filter(|e| e["client"] == json!("guarded")) {
        assert_eq!(entry["carrier"], json!("ask"), "{entry}");
        assert_ne!(entry["decision"], json!("allowed"), "{entry}");
    }
}

/// The carrier itself refuses Operator, independently of the parse boundary.
#[test]
fn a_peer_carrier_refuses_operator_even_if_asked_directly() {
    let dir = std::env::temp_dir().join(format!("mutiny-op-guard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        Overrides::default(),
    );
    let err = ctrl
        .start_http_peer_listener(
            &dir.join("peer.sock"),
            "token",
            "sneaky",
            ConsentMode::Operator,
        )
        .expect_err("a network carrier must never assign Operator");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("Operator"), "{err}");
    // The accepted modes still bind.
    for mode in [
        ConsentMode::Ask,
        ConsentMode::Paired,
        ConsentMode::ObserveOnly,
    ] {
        ctrl.start_http_peer_listener(
            &dir.join(format!("peer-{mode:?}.sock")),
            "token",
            "ok",
            mode,
        )
        .expect("a non-operator mode must bind");
    }
    ctrl.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}
