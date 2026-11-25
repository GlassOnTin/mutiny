//! End-to-end M3.2 tests:
//! 1. `enroll-haven-signer` against a mock Haven HTTP MCP server
//!    (`get_egress_signing_secret`), `0600` persistence, redaction of
//!    `signing_secret_b64` in `list-peers` / `list_peers`, and `PolicyChange`
//!    consent enforcement.
//! 2. Unfocused `PendingConsent` relay + fail-closed HMAC-SHA256 verification
//!    across Tier-2 (`split`) and Tier-4 `ConsentTier::Irreversible`
//!    (`end-session`), proving valid HMAC approval, strict one-shot enforcement
//!    when Haven returns `"session"` on `end-session`, and fail-closed `Deny`
//!    on tampered signature, wrong `sandbox_id`, wrong `request_id`, expired
//!    `expiry_ms`, and replayed `(grant_id, nonce)`.
//! 3. First-human-wins race: local GTK4/controller Deny while a remote Haven
//!    consent call is in flight resolves immediately and ignores the late
//!    remote approval; and valid remote approval dismisses the local GTK4
//!    consent bar under Xvfb.
//! 4. Mock Haven `get_pending_auth_prompt` -> GTK4 Haven Auth Bar visibility +
//!    physical widget `answer_auth_prompt` dispatch + immediate clearing of
//!    `PasswordEntry` under Xvfb.
//! 5. `insert-peer-totp` writing a fresh 6-digit code from mock Haven
//!    (`list_totp_secrets` + `generate_totp_code`, including `secondsRemaining <= 2`
//!    rollover retry) into a live PTY pane, plus strict refusal on
//!    `CarrierRole::Peer`.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use mutiny::action::{ActionId, ConsentTier};
use mutiny::app::{AppController, ConsentResolution};
use mutiny::config::Overrides;
use mutiny::crypto;
use mutiny::http_mcp;
use mutiny::ipc::{
    AuditDecision, ConsentMode, IpcRequest, IpcResponse, PeerIdentity, send_request, server_uid,
};
use mutiny::mcp::{McpBridge, tool_definitions};
use mutiny::mux::MuxKind;
use mutiny::peer_client::PeerPollConfig;
use mutiny::peers;
use mutiny::topology::SplitAxis;
use serde_json::{Value, json};

const TEST_BEARER_TOKEN: &str = "hvn_m32_secret_bearer_token";
const TEST_SIGNING_SECRET: [u8; 32] = [0x5a; 32];

fn test_signing_secret_b64() -> String {
    base64::engine::general_purpose::STANDARD.encode(TEST_SIGNING_SECRET)
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mutiny-m32-{tag}-{}", std::process::id()));
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

#[derive(Clone)]
enum MockConsentBehavior {
    ValidOnce,
    ValidSession,
    Deny,
    TamperedSignature,
    WrongSandboxId,
    WrongRequestId,
    Expired,
    CustomGrantNonce {
        decision: &'static str,
        grant_id: String,
        nonce: String,
    },
    DelayedSession(Duration),
}

#[derive(Default)]
struct MockHavenState {
    consent_queue: VecDeque<MockConsentBehavior>,
    observed_consents: Vec<Value>,
    pending_auth: Option<(String, String, bool)>,
    answered_auth: Vec<(String, bool)>,
    totp_secrets: Vec<Value>,
    totp_responses: VecDeque<(String, u64)>,
}

struct MockHavenM32 {
    addr: SocketAddr,
    state: Arc<Mutex<MockHavenState>>,
    consent_hits: Arc<AtomicUsize>,
    totp_gen_hits: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl MockHavenM32 {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(MockHavenState::default()));
        let consent_hits = Arc::new(AtomicUsize::new(0));
        let totp_gen_hits = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));

        let st_cl = Arc::clone(&state);
        let hits_cl = Arc::clone(&consent_hits);
        let totp_cl = Arc::clone(&totp_gen_hits);
        let shut_cl = Arc::clone(&shutdown);

        let join = thread::spawn(move || {
            while !shut_cl.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let st_conn = Arc::clone(&st_cl);
                        let hits_conn = Arc::clone(&hits_cl);
                        let totp_conn = Arc::clone(&totp_cl);
                        // Handle each HTTP request on its own short thread so a
                        // deliberately delayed `request_egress_grant` does not
                        // block concurrent liveness polls.
                        thread::spawn(move || {
                            let _ = handle_haven_m32(&mut stream, &st_conn, &hits_conn, &totp_conn);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(8));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            state,
            consent_hits,
            totp_gen_hits,
            shutdown,
            join: Some(join),
        }
    }
}

impl Drop for MockHavenM32 {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn wrap_haven_tool_result(structured: Value) -> Value {
    let text = structured.to_string();
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": false
    })
}

fn handle_haven_m32(
    stream: &mut TcpStream,
    state: &Arc<Mutex<MockHavenState>>,
    consent_hits: &Arc<AtomicUsize>,
    totp_gen_hits: &Arc<AtomicUsize>,
) -> std::io::Result<()> {
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

    let expected_hdr = format!("Bearer {TEST_BEARER_TOKEN}");
    let (status, resp_val) = match (method, auth.as_deref()) {
        ("initialize", _) => (
            200u16,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "haven", "version": "5.94.73" },
                    "_meta": { "sh.haven/pairingToken": TEST_BEARER_TOKEN }
                }
            }),
        ),
        ("tools/list", Some(hdr)) if hdr == expected_hdr => (
            200,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        { "name": "get_app_info" },
                        { "name": "get_egress_signing_secret" },
                        { "name": "request_egress_grant" },
                        { "name": "get_pending_auth_prompt" },
                        { "name": "answer_auth_prompt" },
                        { "name": "list_totp_secrets" },
                        { "name": "generate_totp_code" }
                    ]
                }
            }),
        ),
        ("tools/call", Some(hdr)) if hdr == expected_hdr => {
            let tool = val["params"]["name"].as_str().unwrap_or("");
            let args = val["params"]["arguments"].clone();
            let result = match tool {
                "get_app_info" => wrap_haven_tool_result(json!({
                    "versionName": "5.94.73",
                    "device": "pixel"
                })),
                "get_egress_signing_secret" => wrap_haven_tool_result(json!({
                    "algorithm": "HMAC-SHA256",
                    "key_id": "haven-hmac-v1",
                    "secret_base64": test_signing_secret_b64()
                })),
                "request_egress_grant" => {
                    let seq = consent_hits.fetch_add(1, Ordering::SeqCst) + 1;
                    let behavior = {
                        let mut guard = state.lock().unwrap();
                        guard.observed_consents.push(args.clone());
                        guard
                            .consent_queue
                            .pop_front()
                            .unwrap_or(MockConsentBehavior::Deny)
                    };
                    if let MockConsentBehavior::DelayedSession(d) = behavior {
                        thread::sleep(d);
                    }
                    let req_id = args["request_id"].as_str().unwrap_or("").to_owned();
                    let sandbox_id = args["sandbox_id"].as_str().unwrap_or("").to_owned();
                    let host = args["host"].as_str().unwrap_or("").to_owned();
                    let port = args["port"].as_u64().unwrap_or(1) as u16;
                    let future_exp = now_epoch_ms() + 60_000;
                    let grant_id = format!("grant-{seq}");
                    let nonce = format!("nonce-{seq:016x}");

                    let signed_json = match behavior {
                        MockConsentBehavior::ValidOnce => {
                            let sig = crypto::sign_egress_decision(
                                &TEST_SIGNING_SECRET,
                                &req_id,
                                &sandbox_id,
                                &host,
                                port,
                                "once",
                                Some(future_exp),
                                &grant_id,
                                &nonce,
                            );
                            json!({
                                "decision": "once",
                                "request_id": req_id,
                                "sandbox_id": sandbox_id,
                                "host": host,
                                "port": port,
                                "expiry_ms": future_exp,
                                "grant_id": grant_id,
                                "nonce": nonce,
                                "algorithm": "HMAC-SHA256",
                                "key_id": "haven-hmac-v1",
                                "signature": sig
                            })
                        }
                        MockConsentBehavior::ValidSession
                        | MockConsentBehavior::DelayedSession(_) => {
                            let sig = crypto::sign_egress_decision(
                                &TEST_SIGNING_SECRET,
                                &req_id,
                                &sandbox_id,
                                &host,
                                port,
                                "session",
                                Some(future_exp),
                                &grant_id,
                                &nonce,
                            );
                            json!({
                                "decision": "session",
                                "request_id": req_id,
                                "sandbox_id": sandbox_id,
                                "host": host,
                                "port": port,
                                "expiry_ms": future_exp,
                                "grant_id": grant_id,
                                "nonce": nonce,
                                "algorithm": "HMAC-SHA256",
                                "key_id": "haven-hmac-v1",
                                "signature": sig
                            })
                        }
                        MockConsentBehavior::Deny => {
                            let sig = crypto::sign_egress_decision(
                                &TEST_SIGNING_SECRET,
                                &req_id,
                                &sandbox_id,
                                &host,
                                port,
                                "deny",
                                None,
                                &grant_id,
                                &nonce,
                            );
                            json!({
                                "decision": "deny",
                                "request_id": req_id,
                                "sandbox_id": sandbox_id,
                                "host": host,
                                "port": port,
                                "expiry_ms": Value::Null,
                                "grant_id": grant_id,
                                "nonce": nonce,
                                "algorithm": "HMAC-SHA256",
                                "key_id": "haven-hmac-v1",
                                "signature": sig
                            })
                        }
                        MockConsentBehavior::TamperedSignature => {
                            let wrong_key = [0x99u8; 32];
                            let bad_sig = crypto::sign_egress_decision(
                                &wrong_key,
                                &req_id,
                                &sandbox_id,
                                &host,
                                port,
                                "once",
                                Some(future_exp),
                                &grant_id,
                                &nonce,
                            );
                            json!({
                                "decision": "once",
                                "request_id": req_id,
                                "sandbox_id": sandbox_id,
                                "host": host,
                                "port": port,
                                "expiry_ms": future_exp,
                                "grant_id": grant_id,
                                "nonce": nonce,
                                "algorithm": "HMAC-SHA256",
                                "key_id": "haven-hmac-v1",
                                "signature": bad_sig
                            })
                        }
                        MockConsentBehavior::WrongSandboxId => {
                            let wrong_sbx = "mutiny:list:observe:spoofed";
                            let sig = crypto::sign_egress_decision(
                                &TEST_SIGNING_SECRET,
                                &req_id,
                                wrong_sbx,
                                &host,
                                port,
                                "once",
                                Some(future_exp),
                                &grant_id,
                                &nonce,
                            );
                            json!({
                                "decision": "once",
                                "request_id": req_id,
                                "sandbox_id": wrong_sbx,
                                "host": host,
                                "port": port,
                                "expiry_ms": future_exp,
                                "grant_id": grant_id,
                                "nonce": nonce,
                                "algorithm": "HMAC-SHA256",
                                "key_id": "haven-hmac-v1",
                                "signature": sig
                            })
                        }
                        MockConsentBehavior::WrongRequestId => {
                            let wrong_req = "mty-consent-00000000000000000000000000000000";
                            let sig = crypto::sign_egress_decision(
                                &TEST_SIGNING_SECRET,
                                wrong_req,
                                &sandbox_id,
                                &host,
                                port,
                                "once",
                                Some(future_exp),
                                &grant_id,
                                &nonce,
                            );
                            json!({
                                "decision": "once",
                                "request_id": wrong_req,
                                "sandbox_id": sandbox_id,
                                "host": host,
                                "port": port,
                                "expiry_ms": future_exp,
                                "grant_id": grant_id,
                                "nonce": nonce,
                                "algorithm": "HMAC-SHA256",
                                "key_id": "haven-hmac-v1",
                                "signature": sig
                            })
                        }
                        MockConsentBehavior::Expired => {
                            let past_exp = 1_000u64;
                            let sig = crypto::sign_egress_decision(
                                &TEST_SIGNING_SECRET,
                                &req_id,
                                &sandbox_id,
                                &host,
                                port,
                                "once",
                                Some(past_exp),
                                &grant_id,
                                &nonce,
                            );
                            json!({
                                "decision": "once",
                                "request_id": req_id,
                                "sandbox_id": sandbox_id,
                                "host": host,
                                "port": port,
                                "expiry_ms": past_exp,
                                "grant_id": grant_id,
                                "nonce": nonce,
                                "algorithm": "HMAC-SHA256",
                                "key_id": "haven-hmac-v1",
                                "signature": sig
                            })
                        }
                        MockConsentBehavior::CustomGrantNonce {
                            decision,
                            grant_id: g_id,
                            nonce: n_val,
                        } => {
                            let sig = crypto::sign_egress_decision(
                                &TEST_SIGNING_SECRET,
                                &req_id,
                                &sandbox_id,
                                &host,
                                port,
                                decision,
                                Some(future_exp),
                                &g_id,
                                &n_val,
                            );
                            json!({
                                "decision": decision,
                                "request_id": req_id,
                                "sandbox_id": sandbox_id,
                                "host": host,
                                "port": port,
                                "expiry_ms": future_exp,
                                "grant_id": g_id,
                                "nonce": n_val,
                                "algorithm": "HMAC-SHA256",
                                "key_id": "haven-hmac-v1",
                                "signature": sig
                            })
                        }
                    };
                    wrap_haven_tool_result(signed_json)
                }
                "get_pending_auth_prompt" => {
                    let pending = state.lock().unwrap().pending_auth.clone();
                    match pending {
                        Some((profile_id, label, requires_passphrase)) => {
                            wrap_haven_tool_result(json!({
                                "pending": true,
                                "profileId": profile_id,
                                "label": label,
                                "requiresPassphrase": requires_passphrase
                            }))
                        }
                        None => wrap_haven_tool_result(json!({ "pending": false })),
                    }
                }
                "answer_auth_prompt" => {
                    let pw = args["password"].as_str().unwrap_or("").to_owned();
                    let rem = args["rememberPassword"].as_bool().unwrap_or(false);
                    let mut guard = state.lock().unwrap();
                    guard.answered_auth.push((pw, rem));
                    guard.pending_auth = None;
                    wrap_haven_tool_result(json!({
                        "delivered": true,
                        "rememberPassword": rem
                    }))
                }
                "list_totp_secrets" => {
                    let list = state.lock().unwrap().totp_secrets.clone();
                    wrap_haven_tool_result(json!({
                        "count": list.len(),
                        "secrets": list
                    }))
                }
                "generate_totp_code" => {
                    totp_gen_hits.fetch_add(1, Ordering::SeqCst);
                    let secret_id = args["totpSecretId"].as_str().unwrap_or("").to_owned();
                    let (code, secs_rem) = state
                        .lock()
                        .unwrap()
                        .totp_responses
                        .pop_front()
                        .unwrap_or_else(|| (String::from("000000"), 25));
                    wrap_haven_tool_result(json!({
                        "id": secret_id,
                        "label": "GitHub",
                        "code": code,
                        "period": 30,
                        "secondsRemaining": secs_rem
                    }))
                }
                _ => wrap_haven_tool_result(json!({ "ok": true })),
            };
            (200, json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        }
        _ => (
            401,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32001, "message": "unauthorized" }
            }),
        ),
    };

    let body_text = resp_val.to_string();
    let reason = if status == 200 { "OK" } else { "Unauthorized" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_text}",
        body_text.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

#[test]
fn enroll_haven_signer_persists_0600_redacts_secret_and_enforces_policy_change_consent() {
    let dir = temp_dir("enroll-signer");
    let op_sock = dir.join("mutiny.sock");
    let ag_sock = dir.join("mutiny-agent.sock");
    let audit_path = dir.join("audit.jsonl");
    let cfg_path = dir.join("config.toml");
    let peers_path = dir.join("peers.json");
    let mock = MockHavenM32::spawn();

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let op_cl = op_sock.clone();
    let ag_cl = ag_sock.clone();

    let server = thread::spawn(move || {
        let ctrl = AppController::with_config(
            Some((String::from("/bin/sh"), Vec::new())),
            audit_path,
            cfg_path,
            Overrides::default(),
        );
        // Disable unfocused auto-relay here so we also test headless fail-closed
        // PolicyChange rejection on `enroll-haven-signer` when no signer exists yet.
        ctrl.start_ipc_listener(&op_cl).unwrap();
        ctrl.start_agent_listener(&ag_cl, ConsentMode::Ask).unwrap();
        let pid = std::process::id() as i32;
        ctrl.grant_session(
            PeerIdentity {
                uid: server_uid(),
                pid,
                start_time: mutiny::ipc::process_start_time(pid),
                token_peer: None,
            },
            "granted-agent",
        );
        ctrl.create_window(None).unwrap();
        while stop_rx.try_recv().is_err() {
            ctrl.poll_once();
            thread::sleep(Duration::from_millis(4));
        }
        ctrl.shutdown_ipc();
    });

    thread::sleep(Duration::from_millis(50));

    // 1. Pair outbound Haven limb without signer first.
    let url = format!("http://{}/mcp", mock.addr);
    let pair_resp = send_request(
        &op_sock,
        &IpcRequest::PairHaven {
            label: String::from("pixel"),
            url,
            token: Some(String::from(TEST_BEARER_TOKEN)),
            enroll_signer: false,
        },
        Duration::from_secs(5),
    )
    .unwrap();
    match pair_resp {
        IpcResponse::Ok {
            outbound_peer: Some(summary),
            ..
        } => {
            assert!(!summary.has_signing_key);
            assert!(summary.signing_key_id.is_none());
        }
        other => panic!("unexpected PairHaven response: {other:?}"),
    }

    // 2. `enroll-haven-signer` is ConsentTier::PolicyChange: rejected in standing
    //    policies and never rides a standing session grant on an Ask carrier.
    let req_check = IpcRequest::EnrollHavenSigner {
        id_or_label: String::from("pixel"),
    };
    assert_eq!(req_check.consent_tier(), ConsentTier::PolicyChange);

    let bad_policy = send_request(
        &ag_sock,
        &IpcRequest::ProposePolicy {
            description: String::from("try enroll"),
            verbs: vec![String::from("enroll-haven-signer")],
            arg_constraints: Default::default(),
            max_calls_per_minute: 5,
            expires_in_minutes: 5,
        },
        Duration::from_secs(5),
    )
    .unwrap();
    assert!(matches!(bad_policy, IpcResponse::Error { .. }));

    let unapproved_enroll = send_request(&ag_sock, &req_check, Duration::from_secs(5)).unwrap();
    assert!(
        matches!(unapproved_enroll, IpcResponse::Error { .. }),
        "session grant must never bypass enroll-haven-signer: {unapproved_enroll:?}"
    );

    // 3. Operator enrolls Haven signer -> calls `get_egress_signing_secret`,
    //    persists 0600 to `peers.json`, and redacts `signing_secret_b64` in
    //    both `list-peers` IPC and `list_peers` MCP responses.
    let enroll_resp = send_request(&op_sock, &req_check, Duration::from_secs(5)).unwrap();
    match &enroll_resp {
        IpcResponse::Ok {
            outbound_peer: Some(summary),
            ..
        } => {
            assert!(summary.has_signing_key);
            assert_eq!(summary.signing_key_id.as_deref(), Some("haven-hmac-v1"));
        }
        other => panic!("expected EnrollHavenSigner Ok, got {other:?}"),
    }

    let secret_b64 = test_signing_secret_b64();
    let enroll_wire = serde_json::to_string(&enroll_resp).unwrap();
    assert!(
        !enroll_wire.contains(&secret_b64),
        "EnrollHavenSigner response must redact signing_secret_b64: {enroll_wire}"
    );

    let mode = std::fs::metadata(&peers_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let loaded = peers::load(&peers_path).unwrap();
    assert_eq!(loaded.outbound_peers.len(), 1);
    assert_eq!(
        loaded.outbound_peers[0].signing_secret_b64.as_deref(),
        Some(secret_b64.as_str())
    );
    assert_eq!(
        loaded.outbound_peers[0].signing_key_id.as_deref(),
        Some("haven-hmac-v1")
    );

    let ipc_list = send_request(&op_sock, &IpcRequest::ListPeers, Duration::from_secs(5)).unwrap();
    let ipc_list_json = serde_json::to_string(&ipc_list).unwrap();
    assert!(!ipc_list_json.contains(&secret_b64));
    assert!(!ipc_list_json.contains("signing_secret_b64"));
    assert!(ipc_list_json.contains("\"has_signing_key\":true"));
    assert!(ipc_list_json.contains("\"haven-hmac-v1\""));

    let mut bridge = McpBridge::new(
        ag_sock.clone(),
        ConsentMode::ObserveOnly,
        String::from("inspector"),
    );
    let mcp_line = format!(
        "{}\n",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_peers","arguments":{}}})
    );
    let mut reader = Cursor::new(mcp_line.into_bytes());
    let mut out = Vec::new();
    bridge.serve(&mut reader, &mut out).unwrap();
    let mcp_out = String::from_utf8(out).unwrap();
    assert!(!mcp_out.contains(&secret_b64));
    assert!(!mcp_out.contains(TEST_BEARER_TOKEN));
    assert!(mcp_out.contains("haven-hmac-v1"));

    let _ = stop_tx.send(());
    server.join().unwrap();
    drop(mock);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unfocused_consent_relays_hmac_across_tiers_enforces_irreversible_oneshot_and_fails_closed() {
    let dir = temp_dir("hmac-consent");
    let op_sock = dir.join("mutiny.sock");
    let ag_sock = dir.join("mutiny-agent.sock");
    let audit_path = dir.join("audit.jsonl");
    let cfg_path = dir.join("config.toml");
    let killed_log = dir.join("killed-sessions.log");
    let fake_tmux = dir.join("fake-tmux.sh");

    std::fs::write(
        &fake_tmux,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {:?}\nexit 0\n",
            killed_log.display().to_string()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        &cfg_path,
        format!(
            "[sessions]\ntmux_program = {:?}\nrefresh_interval_s = 0\n\n[peer]\nrelay_unfocused_consent = true\nunfocused_grace_ms = 15\n",
            fake_tmux.display().to_string()
        ),
    )
    .unwrap();

    let mock = MockHavenM32::spawn();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (signer_ready_tx, signer_ready_rx) = mpsc::channel::<()>();
    let op_cl = op_sock.clone();
    let ag_cl = ag_sock.clone();

    let server = thread::spawn(move || {
        let ctrl = AppController::with_config(
            Some((String::from("/bin/sh"), Vec::new())),
            audit_path,
            cfg_path,
            Overrides::default(),
        );
        ctrl.set_force_window_focused_for_test(Some(false));
        ctrl.set_peer_poll_config(PeerPollConfig {
            connected_interval: Duration::from_secs(30),
            offline_initial_backoff: Duration::from_millis(50),
            offline_max_backoff: Duration::from_millis(200),
            allow_lan: false,
        });
        ctrl.start_ipc_listener(&op_cl).unwrap();
        ctrl.start_agent_listener(&ag_cl, ConsentMode::Ask).unwrap();
        ctrl.create_window(None).unwrap();

        let mut notified_ready = false;
        while stop_rx.try_recv().is_err() {
            ctrl.poll_once();
            if !notified_ready && ctrl.has_connected_haven_signer() {
                notified_ready = true;
                let _ = signer_ready_tx.send(());
            }
            thread::sleep(Duration::from_millis(4));
        }
        ctrl.shutdown_ipc();
    });

    thread::sleep(Duration::from_millis(50));

    // Pair + enroll signer in one step (`enroll_signer: true`).
    let pair = send_request(
        &op_sock,
        &IpcRequest::PairHaven {
            label: String::from("pixel"),
            url: format!("http://{}/mcp", mock.addr),
            token: Some(String::from(TEST_BEARER_TOKEN)),
            enroll_signer: true,
        },
        Duration::from_secs(5),
    )
    .unwrap();
    assert!(matches!(
        pair,
        IpcResponse::Ok {
            outbound_peer: Some(ref s),
            ..
        } if s.has_signing_key
    ));
    signer_ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("Haven signer connected");

    // 1. Tier-2 `Split` on unfocused `Ask` carrier -> auto-relayed to Haven ->
    //    valid HMAC `"once"` signature approves and creates the split pane!
    mock.state
        .lock()
        .unwrap()
        .consent_queue
        .push_back(MockConsentBehavior::ValidOnce);
    let split_resp = send_request(
        &ag_sock,
        &IpcRequest::Split {
            pane: None,
            axis: SplitAxis::Horizontal,
            cwd: None,
        },
        Duration::from_secs(5),
    )
    .unwrap();
    assert!(
        matches!(split_resp, IpcResponse::Ok { pane: Some(_), .. }),
        "valid Haven HMAC must approve Tier-2 Split: {split_resp:?}"
    );
    {
        let guard = mock.state.lock().unwrap();
        let req_args = guard.observed_consents.last().expect("consent sent");
        assert!(
            req_args["request_id"]
                .as_str()
                .unwrap()
                .starts_with("mty-consent-")
        );
        assert!(
            req_args["sandbox_id"]
                .as_str()
                .unwrap()
                .starts_with("mutiny:split:act:")
        );
        assert_eq!(req_args["host"], json!("mutiny.local"));
        assert_eq!(req_args["port"], json!(1));
        assert_eq!(req_args["scheme"], json!("https"));
    }

    // 2. Tier-4 `ConsentTier::Irreversible` (`EndSession`) when Haven signs
    //    `decision == "session"`: approves ONLY that single `work-alpha` call
    //    and MUST NOT create a session bypass for a second `EndSession`!
    {
        let mut guard = mock.state.lock().unwrap();
        guard
            .consent_queue
            .push_back(MockConsentBehavior::ValidSession);
        guard.consent_queue.push_back(MockConsentBehavior::Deny);
    }
    let end_first = send_request(
        &ag_sock,
        &IpcRequest::EndSession {
            kind: MuxKind::Tmux,
            name: String::from("work-alpha"),
            host: None,
        },
        Duration::from_secs(5),
    )
    .unwrap();
    assert!(
        matches!(end_first, IpcResponse::Ok { .. }),
        "signed Haven response must approve Tier-4 EndSession: {end_first:?}"
    );
    let killed_after_first = std::fs::read_to_string(&killed_log).unwrap_or_default();
    assert!(
        killed_after_first.contains("work-alpha"),
        "expected work-alpha killed, got {killed_after_first:?}"
    );

    let hits_before_second = mock.consent_hits.load(Ordering::SeqCst);
    let end_second = send_request(
        &ag_sock,
        &IpcRequest::EndSession {
            kind: MuxKind::Tmux,
            name: String::from("work-beta"),
            host: None,
        },
        Duration::from_secs(5),
    )
    .unwrap();
    assert!(
        matches!(end_second, IpcResponse::Error { .. }),
        "session decision on Irreversible must NEVER bypass a second EndSession: {end_second:?}"
    );
    assert_eq!(
        mock.consent_hits.load(Ordering::SeqCst),
        hits_before_second + 1,
        "second EndSession must have required a fresh Haven consent prompt"
    );
    let killed_after_second = std::fs::read_to_string(&killed_log).unwrap_or_default();
    assert!(
        !killed_after_second.contains("work-beta"),
        "denied second EndSession must not execute"
    );

    // 3. Fail-closed verification: tampered signature, wrong sandbox_id, wrong
    //    request_id, expired expiry_ms, and replayed (grant_id, nonce) all fail
    //    closed as `Deny`!
    {
        let mut guard = mock.state.lock().unwrap();
        guard
            .consent_queue
            .push_back(MockConsentBehavior::TamperedSignature);
        guard
            .consent_queue
            .push_back(MockConsentBehavior::WrongSandboxId);
        guard
            .consent_queue
            .push_back(MockConsentBehavior::WrongRequestId);
        guard.consent_queue.push_back(MockConsentBehavior::Expired);
        // First use of ("grant-replay-1", "nonce-replay-1") succeeds ("once"):
        guard
            .consent_queue
            .push_back(MockConsentBehavior::CustomGrantNonce {
                decision: "once",
                grant_id: String::from("grant-replay-1"),
                nonce: String::from("nonce-replay-1"),
            });
        // Second use of the exact same ("grant-replay-1", "nonce-replay-1")
        // (even re-signed over the new request_id!) MUST be rejected as replay:
        guard
            .consent_queue
            .push_back(MockConsentBehavior::CustomGrantNonce {
                decision: "once",
                grant_id: String::from("grant-replay-1"),
                nonce: String::from("nonce-replay-1"),
            });
    }

    for label in [
        "tampered-signature",
        "wrong-sandbox-id",
        "wrong-request-id",
        "expired-expiry-ms",
    ] {
        let resp =
            send_request(&ag_sock, &IpcRequest::SaveSession, Duration::from_secs(5)).unwrap();
        assert!(
            matches!(resp, IpcResponse::Error { .. }),
            "{label} must fail closed as Deny, got {resp:?}"
        );
    }

    // First fresh ("grant-replay-1", "nonce-replay-1") succeeds:
    let first_custom =
        send_request(&ag_sock, &IpcRequest::SaveSession, Duration::from_secs(5)).unwrap();
    assert!(
        matches!(first_custom, IpcResponse::Ok { .. }),
        "fresh grant_id/nonce should succeed: {first_custom:?}"
    );

    // Replayed ("grant-replay-1", "nonce-replay-1") fails closed as Deny:
    let replayed_custom =
        send_request(&ag_sock, &IpcRequest::SaveSession, Duration::from_secs(5)).unwrap();
    assert!(
        matches!(replayed_custom, IpcResponse::Error { .. }),
        "replayed (grant_id, nonce) must fail closed as Deny: {replayed_custom:?}"
    );

    // Verify all 5 rejected attempts were recorded as AuditDecision::Denied.
    if let IpcResponse::Ok {
        audit: Some(entries),
        ..
    } = send_request(
        &op_sock,
        &IpcRequest::AuditLog { limit: 30 },
        Duration::from_secs(5),
    )
    .unwrap()
    {
        let denied_saves = entries
            .iter()
            .filter(|e| e.verb == "save-session" && e.decision == AuditDecision::Denied)
            .count();
        assert_eq!(denied_saves, 5, "expected 5 denied save-session entries");
    } else {
        panic!("expected audit log");
    }

    let _ = stop_tx.send(());
    server.join().unwrap();
    drop(mock);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn first_human_wins_race_local_decision_beats_inflight_remote_and_remote_hides_gtk_bar() {
    let dir = temp_dir("consent-race");
    let op_sock = dir.join("mutiny.sock");
    let ag_sock = dir.join("mutiny-agent.sock");
    let audit_path = dir.join("audit.jsonl");
    let cfg_path = dir.join("config.toml");
    std::fs::write(
        &cfg_path,
        "[peer]\nrelay_unfocused_consent = true\nunfocused_grace_ms = 10\n",
    )
    .unwrap();

    let mock = MockHavenM32::spawn();
    mock.state
        .lock()
        .unwrap()
        .consent_queue
        .push_back(MockConsentBehavior::DelayedSession(Duration::from_millis(
            350,
        )));

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (local_deny_tx, local_deny_rx) = mpsc::channel::<()>();
    let (grants_q_tx, grants_q_rx) = mpsc::channel::<mpsc::Sender<Vec<String>>>();
    let op_cl = op_sock.clone();
    let ag_cl = ag_sock.clone();

    let server = thread::spawn(move || {
        let ctrl = AppController::with_config(
            Some((String::from("/bin/sh"), Vec::new())),
            audit_path,
            cfg_path,
            Overrides::default(),
        );
        ctrl.set_force_window_focused_for_test(Some(false));
        ctrl.start_ipc_listener(&op_cl).unwrap();
        ctrl.start_agent_listener(&ag_cl, ConsentMode::Ask).unwrap();
        ctrl.create_window(None).unwrap();

        while stop_rx.try_recv().is_err() {
            ctrl.poll_once();
            if local_deny_rx.try_recv().is_ok()
                && let Some((cid, ..)) = ctrl.pending_consent_prompt()
            {
                ctrl.resolve_consent(cid, ConsentResolution::Deny);
            }
            if let Ok(reply) = grants_q_rx.try_recv() {
                let _ = reply.send(ctrl.session_grants());
            }
            thread::sleep(Duration::from_millis(4));
        }
        ctrl.shutdown_ipc();
    });

    thread::sleep(Duration::from_millis(50));
    send_request(
        &op_sock,
        &IpcRequest::PairHaven {
            label: String::from("pixel"),
            url: format!("http://{}/mcp", mock.addr),
            token: Some(String::from(TEST_BEARER_TOKEN)),
            enroll_signer: true,
        },
        Duration::from_secs(5),
    )
    .unwrap();

    // Dispatch SaveSession on Ask socket; mock Haven holds the request for 350 ms.
    let ag_for_call = ag_sock.clone();
    let caller = thread::spawn(move || {
        send_request(
            &ag_for_call,
            &IpcRequest::SaveSession,
            Duration::from_secs(5),
        )
        .unwrap()
    });

    // Wait until mock Haven has received `request_egress_grant` in flight.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && mock.consent_hits.load(Ordering::SeqCst) == 0 {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(mock.consent_hits.load(Ordering::SeqCst), 1);

    // Local human clicks Deny while Haven is still thinking!
    local_deny_tx.send(()).unwrap();
    let res = caller.join().unwrap();
    assert!(
        matches!(res, IpcResponse::Error { .. }),
        "local Deny must resolve immediately: {res:?}"
    );

    // Let mock Haven's delayed `"session"` approval arrive ~350 ms later;
    // it MUST be ignored without granting a session!
    thread::sleep(Duration::from_millis(450));
    let (g_tx, g_rx) = mpsc::channel();
    grants_q_tx.send(g_tx).unwrap();
    let grants = g_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        grants.is_empty(),
        "late Haven session reply after local Deny must not record a session grant: {grants:?}"
    );

    let _ = stop_tx.send(());
    server.join().unwrap();
    drop(mock);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gtk_haven_auth_bar_and_send_to_haven_consent_dismissal_work_under_xvfb() {
    let has_display = std::env::var("DISPLAY").is_ok_and(|d| !d.is_empty())
        || std::env::var("WAYLAND_DISPLAY").is_ok_and(|d| !d.is_empty());
    if !has_display || gtk4::init().is_err() {
        eprintln!("skipping GTK Haven Auth Bar & Consent Bar test: no display");
        return;
    }

    use gtk4::gio::prelude::*;

    // Invariant check: no MCP tool accepts or forwards SSH passwords.
    let tools = tool_definitions();
    for t in tools.as_array().unwrap() {
        let name = t["name"].as_str().unwrap();
        assert_ne!(name, "answer_auth_prompt");
        assert_ne!(name, "get_pending_auth_prompt");
    }

    let dir = temp_dir("gtk-auth-bar");
    let ag_sock = dir.join("mutiny-agent.sock");
    let mock = MockHavenM32::spawn();
    {
        let mut st = mock.state.lock().unwrap();
        st.pending_auth = Some((
            String::from("prof-bastion-1"),
            String::from("prod-bastion"),
            true,
        ));
        st.consent_queue.push_back(MockConsentBehavior::ValidOnce);
    }

    let app = gtk4::Application::builder()
        .application_id("dev.mutiny.TestM32Auth")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(gtk4::gio::Cancellable::NONE).unwrap();

    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        Overrides::default(),
    );
    // Treat window as focused so local Consent Bar stays visible until we click
    // "Send to Haven" and see the signed Haven reply dismiss it!
    ctrl.set_force_window_focused_for_test(Some(true));
    ctrl.set_peer_poll_config(PeerPollConfig {
        connected_interval: Duration::from_millis(80),
        offline_initial_backoff: Duration::from_millis(50),
        offline_max_backoff: Duration::from_millis(150),
        allow_lan: false,
    });
    ctrl.attach_gtk_app(&app);
    ctrl.start_agent_listener(&ag_sock, ConsentMode::Ask)
        .unwrap();
    let (win_id, _, _) = ctrl.create_window(None).unwrap();

    let pump = |ctrl: &std::rc::Rc<AppController>, rounds: usize| {
        let ctx = gtk4::glib::MainContext::default();
        for _ in 0..rounds {
            ctrl.poll_once();
            while ctx.iteration(false) {}
            thread::sleep(Duration::from_millis(4));
        }
    };

    // Pair + enroll Haven signer.
    ctrl.begin_pair_haven(
        String::from("pixel"),
        format!("http://{}/mcp", mock.addr),
        Some(String::from(TEST_BEARER_TOKEN)),
        true,
        None,
    )
    .unwrap();

    let ui_win = ctrl.ui_window(win_id).expect("GTK MutinyWindow");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline
        && (!ui_win.is_haven_auth_bar_visible() || !ctrl.has_connected_haven_signer())
    {
        pump(&ctrl, 5);
    }
    assert!(
        ui_win.is_haven_auth_bar_visible(),
        "Haven Auth Bar must appear when get_pending_auth_prompt reports pending=true"
    );
    let bar_text = ui_win.haven_auth_bar_text();
    assert!(bar_text.contains("pixel"), "got {bar_text}");
    assert!(bar_text.contains("SSH key passphrase"), "got {bar_text}");
    assert!(bar_text.contains("prod-bastion"), "got {bar_text}");

    // Submit passphrase via physical GTK4 widget interaction; verify immediate
    // clearing of PasswordEntry and delivery of `answer_auth_prompt`.
    ui_win.unlock_haven_auth_bar_via_ui("hunter2-ssh-key-pass", true);
    assert_eq!(
        ui_win.haven_auth_entry_text(),
        "",
        "PasswordEntry text must be cleared immediately on submit"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && mock.state.lock().unwrap().answered_auth.is_empty() {
        pump(&ctrl, 5);
    }
    let answered = mock.state.lock().unwrap().answered_auth.clone();
    assert_eq!(answered, vec![(String::from("hunter2-ssh-key-pass"), true)]);
    assert!(
        !ui_win.is_haven_auth_bar_visible(),
        "Haven Auth Bar must hide once unlocked"
    );

    // Now trigger a Tier-2 Consent Bar while window is focused, click "Send to
    // Haven", and verify Haven's signed reply resolves the request and hides the
    // local GTK4 Consent Bar!
    let ag_call = thread::spawn(move || {
        send_request(&ag_sock, &IpcRequest::SaveSession, Duration::from_secs(5)).unwrap()
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !ui_win.is_consent_bar_visible() {
        pump(&ctrl, 3);
    }
    assert!(ui_win.is_consent_bar_visible());
    assert!(ui_win.is_send_to_haven_visible());

    ui_win.click_send_to_haven();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && ui_win.is_consent_bar_visible() {
        pump(&ctrl, 5);
    }
    assert!(
        !ui_win.is_consent_bar_visible(),
        "valid signed Haven reply must dismiss local GTK4 Consent Bar"
    );
    assert!(matches!(ag_call.join().unwrap(), IpcResponse::Ok { .. }));

    ctrl.close_window(win_id);
    ctrl.shutdown_ipc();
    drop(mock);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn insert_peer_totp_types_fresh_rollover_code_into_pty_and_refuses_peer_carriers() {
    let dir = temp_dir("peer-totp");
    let op_sock = dir.join("mutiny.sock");
    let ag_sock = dir.join("mutiny-agent.sock");
    let peer_sock = dir.join("mutiny-peer.sock");
    let audit_path = dir.join("audit.jsonl");
    let cfg_path = dir.join("config.toml");

    let mock = MockHavenM32::spawn();
    {
        let mut st = mock.state.lock().unwrap();
        st.totp_secrets.push(json!({
            "id": "totp-1",
            "label": "GitHub",
            "issuer": "GitHub",
            "accountName": "ian@example.com"
        }));
        // First call returns secondsRemaining = 1 (<= 2 rollover boundary!),
        // forcing HavenPeerClient to sleep 2.5 s and fetch the fresh next code!
        st.totp_responses.push_back((String::from("111111"), 1));
        st.totp_responses.push_back((String::from("654321"), 29));
    }

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<(SocketAddr, mutiny::topology::PaneId)>();
    let op_cl = op_sock.clone();
    let ag_cl = ag_sock.clone();
    let peer_cl = peer_sock.clone();

    let server = thread::spawn(move || {
        // Spawn `cat` in the PTY so anything written to the PTY master is echoed
        // cleanly onto the pane screen without executing shell commands.
        let ctrl = AppController::with_config(
            Some((String::from("/bin/cat"), Vec::new())),
            audit_path,
            cfg_path,
            Overrides::default(),
        );
        ctrl.start_ipc_listener(&op_cl).unwrap();
        ctrl.start_agent_listener(&ag_cl, ConsentMode::Paired)
            .unwrap();
        ctrl.start_http_peer_listener(
            &peer_cl,
            "paired-peer-tok",
            "remote-agent",
            ConsentMode::Paired,
        )
        .unwrap();
        let (_, _, pane_id) = ctrl.create_window(None).unwrap();

        let http = http_mcp::spawn_pairable(
            "127.0.0.1:0".parse().unwrap(),
            ctrl.inbound_peer_table(),
            None,
        )
        .unwrap();
        ready_tx.send((http.local_addr, pane_id)).unwrap();

        while stop_rx.try_recv().is_err() {
            ctrl.poll_once();
            thread::sleep(Duration::from_millis(4));
        }
        ctrl.shutdown_ipc();
    });

    let (http_addr, pane_id) = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();

    // Pair outbound Haven limb.
    send_request(
        &op_sock,
        &IpcRequest::PairHaven {
            label: String::from("pixel"),
            url: format!("http://{}/mcp", mock.addr),
            token: Some(String::from(TEST_BEARER_TOKEN)),
            enroll_signer: false,
        },
        Duration::from_secs(5),
    )
    .unwrap();

    // 1. Strict Security Invariant: `CarrierRole::Peer` (both local agent socket
    //    in `Paired` mode AND remote HTTP MCP in `Paired` mode) MUST be refused!
    let peer_ipc = send_request(
        &ag_sock,
        &IpcRequest::RunAction {
            action: ActionId::InsertPeerTotp,
            window: None,
        },
        Duration::from_secs(5),
    )
    .unwrap();
    assert!(
        matches!(peer_ipc, IpcResponse::Error { ref message } if message.contains("Operator")),
        "local Peer socket must refuse insert-peer-totp: {peer_ipc:?}"
    );

    let (st_http, body_http) = raw_http_post(
        http_addr,
        Some("paired-peer-tok"),
        &json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "run_action",
                "arguments": { "slug": "insert-peer-totp" }
            }
        })
        .to_string(),
    );
    assert_eq!(st_http, 200);
    let http_val: Value = serde_json::from_str(&body_http).unwrap();
    assert_eq!(
        http_val["result"]["isError"],
        json!(true),
        "HTTP MCP Peer carrier must refuse insert-peer-totp: {http_val}"
    );
    assert_eq!(
        mock.totp_gen_hits.load(Ordering::SeqCst),
        0,
        "refused Peer calls must never reach Haven generate_totp_code"
    );

    // 2. Local `Operator` triggers `insert-peer-totp`: queries `list_totp_secrets`,
    //    sees `secondsRemaining == 1` on `"111111"`, sleeps 2.5 s for rollover,
    //    fetches fresh `"654321"`, and writes it directly into the focused PTY!
    let op_totp = send_request(
        &op_sock,
        &IpcRequest::RunAction {
            action: ActionId::InsertPeerTotp,
            window: None,
        },
        Duration::from_secs(5),
    )
    .unwrap();
    assert!(matches!(op_totp, IpcResponse::Ok { .. }));

    let deadline = Instant::now() + Duration::from_secs(6);
    let mut screen = String::new();
    while Instant::now() < deadline {
        if let IpcResponse::Ok {
            text: Some(txt), ..
        } = send_request(
            &op_sock,
            &IpcRequest::ScreenText {
                pane: Some(pane_id),
            },
            Duration::from_secs(3),
        )
        .unwrap()
        {
            screen = txt;
            if screen.contains("654321") {
                break;
            }
        }
        thread::sleep(Duration::from_millis(80));
    }
    assert!(
        screen.contains("654321"),
        "expected fresh rollover TOTP code '654321' in PTY screen, got: {screen:?}"
    );
    assert!(
        !screen.contains("111111"),
        "expiring code '111111' (secondsRemaining <= 2) must not be typed: {screen:?}"
    );
    assert_eq!(mock.totp_gen_hits.load(Ordering::SeqCst), 2);

    let _ = stop_tx.send(());
    server.join().unwrap();
    drop(mock);
    let _ = std::fs::remove_dir_all(&dir);
}
