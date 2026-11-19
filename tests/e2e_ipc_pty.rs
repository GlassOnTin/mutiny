use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use mutiny::action::ActionId;
use mutiny::app::AppController;
use mutiny::ipc::{ConsentMode, IpcRequest, IpcResponse, PeerIdentity, send_request, server_uid};
use mutiny::mcp::{McpBridge, tool_definitions};
use mutiny::topology::{PaneId, SplitAxis};
use serde_json::{Value, json};
use std::io::Cursor;

/// A headless controller with both carriers bound, plus a channel to re-pin
/// the agent mode the way the operator's launch flag would.
struct TestServer {
    operator_sock: PathBuf,
    agent_sock: PathBuf,
    audit_path: PathBuf,
    dir: PathBuf,
    first_pane: PaneId,
    mode_tx: mpsc::Sender<ConsentMode>,
    grant_tx: mpsc::Sender<(PeerIdentity, String)>,
    stop_tx: mpsc::Sender<()>,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    fn spawn(tag: &str, initial_agent_mode: ConsentMode) -> Self {
        let dir = env::temp_dir().join(format!("mutiny-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("test dir");
        let operator_sock = dir.join("mutiny.sock");
        let agent_sock = dir.join("mutiny-agent.sock");
        let audit_path = dir.join("audit.jsonl");

        let (ready_tx, ready_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel();
        let (mode_tx, mode_rx) = mpsc::channel::<ConsentMode>();
        let (grant_tx, grant_rx) = mpsc::channel::<(PeerIdentity, String)>();

        let op = operator_sock.clone();
        let ag = agent_sock.clone();
        let audit = audit_path.clone();
        let handle = thread::spawn(move || {
            let ctrl =
                AppController::with_audit_path(Some((String::from("/bin/sh"), Vec::new())), audit);
            ctrl.start_ipc_listener(&op).expect("operator listener");
            ctrl.start_agent_listener(&ag, initial_agent_mode)
                .expect("agent listener");
            let (_w, _t, p1) = ctrl.create_window(None).expect("spawn first PTY");
            ready_tx.send(p1).unwrap();

            while stop_rx.try_recv().is_err() {
                if let Ok(mode) = mode_rx.try_recv() {
                    ctrl.set_agent_mode(mode);
                }
                if let Ok((peer, label)) = grant_rx.try_recv() {
                    ctrl.grant_session(peer, &label);
                }
                ctrl.poll_once();
                thread::sleep(Duration::from_millis(5));
            }
            ctrl.shutdown_ipc();
        });

        let first_pane = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("server ready");

        Self {
            operator_sock,
            agent_sock,
            audit_path,
            dir,
            first_pane,
            mode_tx,
            grant_tx,
            stop_tx,
            handle: Some(handle),
        }
    }

    fn set_agent_mode(&self, mode: ConsentMode) {
        self.mode_tx.send(mode).expect("mode channel");
        thread::sleep(Duration::from_millis(30));
    }

    /// Stands in for the operator tapping Allow Session on the consent bar.
    fn grant_session(&self, peer: PeerIdentity, label: &str) {
        self.grant_tx
            .send((peer, label.to_owned()))
            .expect("grant channel");
        thread::sleep(Duration::from_millis(30));
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Writes one raw JSONL line and reads one response, bypassing the client
/// helpers so a test can send fields the helpers would never produce.
fn raw_round_trip(sock: &PathBuf, line: &str) -> Value {
    let mut stream = UnixStream::connect(sock).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream.write_all(line.as_bytes()).expect("write");
    stream.write_all(b"\n").expect("newline");
    stream.flush().expect("flush");
    let mut reply = String::new();
    BufReader::new(&mut stream)
        .read_line(&mut reply)
        .expect("read reply");
    serde_json::from_str(reply.trim()).expect("reply is JSON")
}

#[test]
fn live_pty_split_osc133_wait_and_detach_over_unix_socket() {
    let server = TestServer::spawn("pty", ConsentMode::ObserveOnly);
    let sock_path = server.operator_sock.clone();
    let p1 = server.first_pane;

    // 1. Split right over IPC -> spawns real second /bin/sh PTY
    let p2 = match send_request(
        &sock_path,
        &IpcRequest::Split {
            pane: Some(p1),
            axis: SplitAxis::Horizontal,
            cwd: None,
        },
        Duration::from_secs(3),
    )
    .expect("IPC Split")
    {
        IpcResponse::Ok {
            pane: Some(new_pane),
            ..
        } => new_pane,
        other => panic!("unexpected Split response: {other:?}"),
    };
    assert_ne!(p1, p2);

    // 2. Subscribe to WaitPrompt while feeding an OSC 133 command into p2's shell
    let wait_sock = sock_path.clone();
    let waiter = thread::spawn(move || {
        send_request(
            &wait_sock,
            &IpcRequest::WaitPrompt {
                pane: Some(p2),
                since_seq: Some(0),
                timeout_ms: 5_000,
            },
            Duration::from_secs(6),
        )
        .expect("WaitPrompt")
    });

    thread::sleep(Duration::from_millis(40));
    send_request(
        &sock_path,
        &IpcRequest::SendText {
            pane: Some(p2),
            text: String::from(
                "printf '\\033]133;C\\007mutiny-pty-ok\\n\\033]133;D;9\\007\\033]133;A\\007'\n",
            ),
        },
        Duration::from_secs(3),
    )
    .expect("SendText to p2");

    let wait_resp = waiter.join().expect("waiter join");
    match wait_resp {
        IpcResponse::Ok {
            prompt_seq: Some(seq),
            command: Some(cmd),
            ..
        } => {
            assert!(seq >= 1);
            assert_eq!(cmd.exit_code, Some(9));
            assert!(
                cmd.output.contains("mutiny-pty-ok"),
                "unexpected captured output: {:?}",
                cmd.output
            );
        }
        other => panic!("unexpected WaitPrompt response: {other:?}"),
    }

    // 3. Focus p1, then UrgentNext must jump to p2 (which exited 9)
    send_request(
        &sock_path,
        &IpcRequest::Focus { pane: p1 },
        Duration::from_secs(3),
    )
    .expect("Focus p1");
    send_request(
        &sock_path,
        &IpcRequest::RunAction {
            action: ActionId::UrgentNext,
            window: None,
        },
        Duration::from_secs(3),
    )
    .expect("RunAction UrgentNext");

    // 4. Detach p2 into its own window; List shows 2 windows
    let detached_win = match send_request(
        &sock_path,
        &IpcRequest::DetachSplit { pane: Some(p2) },
        Duration::from_secs(3),
    )
    .expect("DetachSplit")
    {
        IpcResponse::Ok {
            window: Some(w), ..
        } => w,
        other => panic!("unexpected DetachSplit response: {other:?}"),
    };

    match send_request(&sock_path, &IpcRequest::List, Duration::from_secs(3)).expect("List") {
        IpcResponse::Ok {
            workspace: Some(ws),
            ..
        } => {
            assert_eq!(ws.windows.len(), 2);
            assert_eq!(ws.active_window, Some(detached_win));
        }
        other => panic!("unexpected List response: {other:?}"),
    }

    match send_request(
        &sock_path,
        &IpcRequest::LastOutput {
            pane: Some(PaneId(p2.0)),
        },
        Duration::from_secs(3),
    )
    .expect("LastOutput after detach")
    {
        IpcResponse::Ok {
            command: Some(cmd), ..
        } => {
            assert_eq!(cmd.exit_code, Some(9));
            assert!(cmd.output.contains("mutiny-pty-ok"));
        }
        other => panic!("unexpected LastOutput response: {other:?}"),
    }
}

#[test]
fn carrier_refuses_client_asserted_consent_mode_on_both_sockets() {
    let server = TestServer::spawn("spoof", ConsentMode::ObserveOnly);

    for sock in [&server.operator_sock, &server.agent_sock] {
        for claimed in ["operator", "paired", "ask"] {
            let line = format!(r#"{{"op":"list","consent_mode":"{claimed}"}}"#);
            let reply = raw_round_trip(sock, &line);
            assert_eq!(
                reply["status"],
                json!("error"),
                "claiming '{claimed}' on {} must be refused, got {reply}",
                sock.display()
            );
            assert!(
                reply["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("assigned by the transport"),
                "unhelpful refusal: {reply}"
            );
        }
    }

    // The same request without the claimed mode succeeds on the operator
    // carrier, which is what makes the refusal above a trust rule and not a
    // blanket rejection of the verb.
    let ok = raw_round_trip(&server.operator_sock, r#"{"op":"list"}"#);
    assert_eq!(ok["status"], json!("ok"));

    // An unsupported wire version is refused too.
    let bad_version = raw_round_trip(&server.operator_sock, r#"{"version":99,"op":"list"}"#);
    assert_eq!(bad_version["status"], json!("error"));
    assert!(
        bad_version["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unsupported IPC protocol version")
    );
}

#[test]
fn agent_carrier_mode_is_the_boundary_not_the_bridge_flag() {
    let server = TestServer::spawn("carrier", ConsentMode::ObserveOnly);
    let p1 = server.first_pane;

    // A bridge that asks for `paired` cannot raise an observe-only carrier.
    let mut escalation = McpBridge::new(
        server.agent_sock.clone(),
        ConsentMode::Paired,
        String::from("would-be-paired"),
    );
    let input = format!(
        "{}\n",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"spawn_pane","arguments":{"mode":"split-right","pane":p1.0}}})
    );
    let mut reader = Cursor::new(input.as_bytes());
    let mut out = Vec::new();
    escalation.serve(&mut reader, &mut out).unwrap();
    let reply: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(reply["result"]["isError"], json!(true));
    assert!(
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("observe-only"),
        "carrier must deny, got {reply}"
    );

    // The denial is audited server-side with the carrier that produced it.
    let audit = raw_round_trip(&server.operator_sock, r#"{"op":"audit-log","limit":50}"#);
    let entries = audit["audit"].as_array().expect("audit array");
    let denied = entries
        .iter()
        .find(|e| e["client"] == json!("would-be-paired") && e["decision"] == json!("denied"))
        .expect("denied entry for the escalation attempt");
    assert_eq!(denied["carrier"], json!("observe-only"));
    assert_eq!(denied["verb"], json!("split"));

    // Operator re-pins the carrier to paired; the same bridge now succeeds.
    server.set_agent_mode(ConsentMode::Paired);
    let mut paired = McpBridge::new(
        server.agent_sock.clone(),
        ConsentMode::Paired,
        String::from("granted-paired"),
    );
    let mut reader = Cursor::new(input.as_bytes());
    let mut out = Vec::new();
    paired.serve(&mut reader, &mut out).unwrap();
    let reply: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(reply["result"]["isError"], json!(false), "got {reply}");
}

#[test]
fn mcp_bridge_drives_agent_turns_resources_and_exited_ring() {
    let server = TestServer::spawn("mcp", ConsentMode::Paired);
    let p1 = server.first_pane;

    let mut paired = McpBridge::new(
        server.agent_sock.clone(),
        ConsentMode::Paired,
        String::from("initial"),
    );
    let session_lines = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","clientInfo":{"name":"haven-peer"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"spawn_pane","arguments":{"mode":"split-right","pane":p1.0}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"send_to_agent","arguments":{"pane":2,"message":"echo HAVEN_TURN_DONE && exit 5"}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"present_notice","arguments":{"pane":p1.0,"message":"Build review ready"}}}),
    ];
    let payload: String = session_lines.iter().map(|v| format!("{v}\n")).collect();
    let mut reader = Cursor::new(payload.into_bytes());
    let mut out = Vec::new();
    paired.serve(&mut reader, &mut out).unwrap();
    let replies: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(replies.len(), 4); // notification produces no reply
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], json!("mutiny"));

    thread::sleep(Duration::from_millis(200));

    let verify_lines = [
        json!({"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"ui://mutiny/exited"}}),
        json!({"jsonrpc":"2.0","id":6,"method":"resources/read","params":{"uri":"ui://mutiny/audit"}}),
        json!({"jsonrpc":"2.0","id":7,"method":"resources/read","params":{"uri":format!("ui://mutiny/pane/{}/snapshot", p1.0)}}),
        json!({"jsonrpc":"2.0","id":8,"method":"resources/read","params":{"uri":"ui://mutiny/actions"}}),
        json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"list_actions","arguments":{}}}),
    ];
    let verify_payload: String = verify_lines.iter().map(|v| format!("{v}\n")).collect();
    let mut v_reader = Cursor::new(verify_payload.into_bytes());
    let mut v_out = Vec::new();
    paired.serve(&mut v_reader, &mut v_out).unwrap();
    let v_replies: Vec<Value> = String::from_utf8(v_out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let exited_json = v_replies[0]["result"]["contents"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        exited_json.contains("HAVEN_TURN_DONE"),
        "expected exited pane screen to preserve output, got: {exited_json}"
    );
    assert!(exited_json.contains("\"exit_code\": 5"));

    let audit_json = v_replies[1]["result"]["contents"][0]["text"]
        .as_str()
        .unwrap();
    assert!(audit_json.contains("\"haven-peer\""));
    assert!(audit_json.contains("\"carrier\""));

    let p1_snap = v_replies[2]["result"]["contents"][0]["text"]
        .as_str()
        .unwrap();
    assert!(p1_snap.contains("Build review ready"));

    // The action catalog is reachable both as a resource and as a tool, so an
    // agent can discover the slugs run_action accepts.
    for reply in [&v_replies[3], &v_replies[4]] {
        let text = reply["result"]["contents"][0]["text"]
            .as_str()
            .or_else(|| reply["result"]["content"][0]["text"].as_str())
            .expect("catalog text");
        assert!(
            text.contains("detach-split"),
            "catalog missing slugs: {text}"
        );
        assert!(text.contains("urgent-next"));
    }

    // Audit survived on disk while the server ran.
    let on_disk = fs::read_to_string(&server.audit_path).expect("audit file");
    assert!(on_disk.contains("haven-peer"));
    assert!(
        !on_disk.contains("HAVEN_TURN_DONE"),
        "payload must be redacted"
    );
}

#[test]
fn audit_trail_survives_a_restart() {
    let dir = env::temp_dir().join(format!("mutiny-restart-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let audit_path = dir.join("audit.jsonl");
    let sock = dir.join("mutiny.sock");

    let (stop_tx, stop_rx) = mpsc::channel();
    let audit_clone = audit_path.clone();
    let sock_clone = sock.clone();
    let first = thread::spawn(move || {
        let ctrl = AppController::with_audit_path(None, audit_clone);
        ctrl.start_ipc_listener(&sock_clone).expect("listener");
        while stop_rx.try_recv().is_err() {
            ctrl.poll_once();
            thread::sleep(Duration::from_millis(5));
        }
        ctrl.shutdown_ipc();
    });

    // No window: an Act verb from an ask-mode agent fails closed, which is
    // still an audited decision. Use the operator carrier for a clean Allowed.
    thread::sleep(Duration::from_millis(50));
    send_request(&sock, &IpcRequest::List, Duration::from_secs(3)).expect("list");
    send_request(&sock, &IpcRequest::Actions, Duration::from_secs(3)).expect("actions");
    stop_tx.send(()).unwrap();
    first.join().unwrap();

    let written = fs::read_to_string(&audit_path).expect("audit written");
    assert_eq!(
        written.lines().count(),
        2,
        "expected two entries: {written}"
    );

    // A fresh controller on the same path reloads the trail into its viewer
    // ring and continues the sequence rather than restarting it. The
    // controller lives on its own thread (Rc is not Send), so it reports what
    // it loaded back over a channel.
    let sock2 = dir.join("mutiny2.sock");
    let (stop2_tx, stop2_rx) = mpsc::channel();
    let (loaded_tx, loaded_rx) = mpsc::channel();
    let audit_clone = audit_path.clone();
    let sock2_clone = sock2.clone();
    let pump = thread::spawn(move || {
        let reborn = AppController::with_audit_path(None, audit_clone);
        let loaded = reborn.audit_entries(50);
        loaded_tx
            .send((
                loaded.len(),
                loaded.iter().map(|e| e.verb.clone()).collect::<Vec<_>>(),
                loaded.iter().map(|e| e.seq).max().unwrap_or(0),
            ))
            .unwrap();
        reborn.start_ipc_listener(&sock2_clone).expect("listener 2");
        while stop2_rx.try_recv().is_err() {
            reborn.poll_once();
            thread::sleep(Duration::from_millis(5));
        }
        reborn.shutdown_ipc();
    });

    let (loaded_len, loaded_verbs, max_seq) = loaded_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("reload report");
    assert_eq!(loaded_len, 2, "trail must reload into the viewer ring");
    assert!(loaded_verbs.contains(&String::from("list")));
    assert!(loaded_verbs.contains(&String::from("actions")));

    thread::sleep(Duration::from_millis(30));
    send_request(&sock2, &IpcRequest::List, Duration::from_secs(3)).expect("list after restart");
    stop2_tx.send(()).unwrap();
    pump.join().unwrap();

    let after = fs::read_to_string(&audit_path).expect("audit after restart");
    assert_eq!(
        after.lines().count(),
        3,
        "new entry appended, old ones kept"
    );
    let seqs: Vec<u64> = after
        .lines()
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["seq"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert!(
        seqs.iter().max().unwrap() > &max_seq,
        "sequence must continue across restart: {seqs:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn every_advertised_mcp_tool_dispatches() {
    let server = TestServer::spawn("dispatch", ConsentMode::Paired);
    let p1 = server.first_pane;

    // Minimal valid arguments per tool. A new advertised tool must be added
    // here, which is the lock that keeps tools/list honest about dispatch.
    let args_for = |name: &str| -> Option<Value> {
        Some(match name {
            "list_workspace"
            | "list_actions"
            | "read_pane_snapshot"
            | "read_screen_text"
            | "read_last_command_output"
            | "read_exited_pane" => json!({}),
            "search_scrollback" => json!({"pattern": "x"}),
            "await_turn" => json!({"timeout_ms": 60}),
            "wait_prompt" => json!({"timeout_ms": 60}),
            "send_terminal_input" => json!({"text": ""}),
            "send_to_agent" => json!({"message": "true"}),
            "spawn_pane" => json!({"mode": "split-right", "pane": p1.0}),
            "detach_split" => json!({"pane": p1.0}),
            "move_pane" => json!({"pane": p1.0, "target_tab": 1}),
            "focus_pane" => json!({"pane": p1.0}),
            "run_action" => json!({"slug": "new-tab"}),
            "present_notice" => json!({"pane": p1.0, "message": "x"}),
            "present_media" => json!({"pane": p1.0, "path": "/nonexistent-probe.png"}),
            _ => return None,
        })
    };

    let names: Vec<String> = tool_definitions()
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();

    let mut bridge = McpBridge::new(
        server.agent_sock.clone(),
        ConsentMode::Paired,
        String::from("dispatch-probe"),
    );

    for (idx, name) in names.iter().enumerate() {
        let args = args_for(name)
            .unwrap_or_else(|| panic!("advertised tool '{name}' has no dispatch probe arguments"));
        let line = format!(
            "{}\n",
            json!({"jsonrpc":"2.0","id":idx+1,"method":"tools/call","params":{"name":name,"arguments":args}})
        );
        let mut reader = Cursor::new(line.into_bytes());
        let mut out = Vec::new();
        bridge.serve(&mut reader, &mut out).unwrap();
        let reply: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
        assert!(
            reply["error"].is_null(),
            "tool '{name}' failed to dispatch: {reply}"
        );
        assert!(
            reply["result"]["content"][0]["text"].is_string(),
            "tool '{name}' returned no content: {reply}"
        );
    }
}

/// A separate process that speaks raw JSONL to a carrier on demand. Grants key
/// on kernel-verified peer identity, and every thread in one process shares a
/// pid, so proving label-spoof refusal requires genuinely distinct processes.
struct PeerProcess {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    pid: u32,
}

impl PeerProcess {
    fn spawn(sock: &PathBuf) -> Self {
        let exe = env::current_exe().expect("current exe");
        let mut child = std::process::Command::new(exe)
            .arg("--exact")
            .arg("raw_peer_helper")
            .arg("--nocapture")
            .env("MUTINY_TEST_PEER_SOCKET", sock)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn peer process");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let pid = child.id();
        Self {
            child,
            stdin,
            stdout,
            pid,
        }
    }

    /// The identity the server will derive for this process, computed from the
    /// same kernel and /proc sources the carrier uses.
    fn identity(&self) -> PeerIdentity {
        PeerIdentity {
            uid: server_uid(),
            pid: self.pid as i32,
            start_time: mutiny::ipc::process_start_time(self.pid as i32),
            token_peer: None,
        }
    }

    fn call(&mut self, line: &str) -> Value {
        writeln!(self.stdin, "{line}").expect("write request");
        self.stdin.flush().expect("flush");
        loop {
            let mut buf = String::new();
            let n = self.stdout.read_line(&mut buf).expect("read peer stdout");
            assert!(n > 0, "peer process exited without replying");
            if let Some(rest) = buf.strip_prefix("MUTINY_PEER_REPLY ") {
                return serde_json::from_str(rest.trim()).expect("peer reply is JSON");
            }
        }
    }
}

impl Drop for PeerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Driven by `PeerProcess` over stdin. Without the env var this is a no-op, so
/// the ordinary suite stays green headless.
#[test]
fn raw_peer_helper() {
    let Ok(sock) = env::var("MUTINY_TEST_PEER_SOCKET") else {
        return;
    };
    let sock = PathBuf::from(sock);
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = raw_round_trip(&sock, &line);
        println!("MUTINY_PEER_REPLY {reply}");
        std::io::stdout().flush().expect("flush stdout");
    }
}

#[test]
fn same_uid_peer_claiming_another_label_cannot_inherit_its_grant() {
    let server = TestServer::spawn("grantspoof", ConsentMode::Ask);
    let p1 = server.first_pane;

    // Both processes claim the identical label. Only one is granted.
    let observe = r#"{"op":"list","client":"trusted-peer"}"#;
    let act = format!(
        r#"{{"op":"present-notice","client":"trusted-peer","pane":{},"message":"x"}}"#,
        p1.0
    );

    let mut peer_a = PeerProcess::spawn(&server.agent_sock);
    let identity_a = peer_a.identity();
    assert!(identity_a.start_time > 0, "starttime must be readable");

    // 1. Peer A is seen and audited under the identity the carrier derived.
    assert_eq!(peer_a.call(observe)["status"], json!("ok"));
    let audit = raw_round_trip(&server.operator_sock, r#"{"op":"audit-log","limit":50}"#);
    let entry_a = audit["audit"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["client"] == json!("trusted-peer"))
        .expect("peer A audited");
    let seen_a: PeerIdentity =
        serde_json::from_value(entry_a["peer"].clone()).expect("peer identity in audit");
    assert_eq!(
        seen_a, identity_a,
        "carrier-derived identity must match the kernel facts about peer A"
    );

    // 2. Operator grants a session to that verified identity.
    server.grant_session(identity_a, "trusted-peer");

    // 3. Peer A reconnects (new socket, same process) and the grant holds.
    let a_act = peer_a.call(&act);
    assert_eq!(
        a_act["status"],
        json!("ok"),
        "granted peer must be allowed on a fresh connection: {a_act}"
    );

    // 4. Peer B is a different process, same uid, claiming the SAME label.
    let mut peer_b = PeerProcess::spawn(&server.agent_sock);
    let identity_b = peer_b.identity();
    assert_eq!(identity_b.uid, identity_a.uid, "both peers are same-uid");
    assert_ne!(identity_b.pid, identity_a.pid);

    let b_act = peer_b.call(&act);
    assert_eq!(
        b_act["status"],
        json!("error"),
        "claiming the granted label must not inherit the grant: {b_act}"
    );
    assert!(
        b_act["message"]
            .as_str()
            .unwrap_or_default()
            .contains("consent prompt requires"),
        "ungranted peer must fail closed headless: {b_act}"
    );

    // 5. The trail shows two distinct processes behind one label, and the
    //    denial is recorded against B's identity rather than A's.
    let audit = raw_round_trip(&server.operator_sock, r#"{"op":"audit-log","limit":50}"#);
    let denied = audit["audit"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["client"] == json!("trusted-peer") && e["decision"] == json!("denied"))
        .expect("denied entry for peer B");
    let seen_b: PeerIdentity =
        serde_json::from_value(denied["peer"].clone()).expect("peer identity");
    assert_eq!(seen_b, identity_b);
    assert_ne!(seen_b, identity_a);
}

#[test]
fn audit_entries_written_before_peer_identity_still_load() {
    let dir = env::temp_dir().join(format!("mutiny-legacy-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let audit_path = dir.join("audit.jsonl");

    // Shaped exactly as the first M0.5 build wrote it: no `peer` field.
    let legacy = json!({
        "seq": 1, "unix_ms": 0, "client": "ctl", "verb": "list",
        "tier": "observe", "carrier": "operator", "decision": "allowed", "detail": ""
    });
    let modern = json!({
        "seq": 2, "unix_ms": 0, "client": "agent", "verb": "list",
        "tier": "observe", "carrier": "ask", "decision": "allowed", "detail": "",
        "peer": {"uid": server_uid(), "pid": 4242, "start_time": 99}
    });
    fs::write(&audit_path, format!("{legacy}\n{modern}\n")).expect("write trail");

    let ctrl = AppController::with_audit_path(None, audit_path.clone());
    let entries = ctrl.audit_entries(10);
    assert_eq!(
        entries.len(),
        2,
        "a pre-upgrade entry must load, not be silently dropped"
    );
    let old = entries.iter().find(|e| e.seq == 1).expect("legacy entry");
    assert_eq!(old.peer, PeerIdentity::default());
    let new = entries.iter().find(|e| e.seq == 2).expect("modern entry");
    assert_eq!(new.peer.pid, 4242);
    assert_eq!(new.peer.start_time, 99);

    let _ = fs::remove_dir_all(&dir);
}

/// The viewport search bar's logic layer, driven headlessly through the same
/// controller methods the widgets call. The widgets themselves need a display.
#[test]
fn viewport_search_progresses_across_ticks_and_navigates() {
    let dir = env::temp_dir().join(format!("mutiny-vsearch-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let ctrl = AppController::with_audit_path(None, dir.join("audit.jsonl"));
    let (_w, _t, p1) = ctrl.create_window(None).expect("window");

    let mut buf = String::new();
    for i in 0..12_000 {
        buf.push_str(&format!("row {i} some searchable content here\r\n"));
    }
    ctrl.workspace
        .borrow_mut()
        .pane_mut(p1)
        .unwrap()
        .advance_bytes(buf.as_bytes());
    let history = ctrl
        .workspace
        .borrow()
        .pane(p1)
        .unwrap()
        .snapshot(None)
        .scrollback_lines;
    assert!(history >= 10_000, "expected full scrollback, got {history}");

    // A no-match query over the whole history must not complete in one tick.
    ctrl.begin_pane_search(Some(p1), "absent-needle-xyz");
    let mut ticks = 0usize;
    loop {
        ctrl.poll_once();
        ticks += 1;
        let done = ctrl
            .workspace
            .borrow()
            .pane(p1)
            .unwrap()
            .search()
            .unwrap()
            .is_done();
        if done {
            break;
        }
        assert!(ticks < 1_000, "scan must terminate");
    }
    assert!(
        ticks > 1,
        "a full-history miss must spread across ticks, finished in {ticks}"
    );
    assert!(
        ctrl.workspace
            .borrow()
            .pane(p1)
            .unwrap()
            .search()
            .unwrap()
            .matches()
            .is_empty()
    );

    // A hit query completes, then navigation moves the cursor and scrolls.
    // The needle must survive the 10 000-line cap: with 12 000 lines emitted,
    // only roughly rows 1970 onward are retained.
    ctrl.begin_pane_search(Some(p1), "row 2500 ");
    for _ in 0..1_000 {
        ctrl.poll_once();
        if ctrl
            .workspace
            .borrow()
            .pane(p1)
            .unwrap()
            .search()
            .unwrap()
            .is_done()
        {
            break;
        }
    }
    let offset_before = ctrl
        .workspace
        .borrow()
        .pane(p1)
        .unwrap()
        .snapshot(None)
        .display_offset;
    ctrl.navigate_pane_search(Some(p1), true);
    let (cursor, offset_after) = {
        let ws = ctrl.workspace.borrow();
        let eng = ws.pane(p1).unwrap();
        (
            eng.search().unwrap().cursor(),
            eng.snapshot(None).display_offset,
        )
    };
    assert_eq!(cursor, Some(0));
    assert_ne!(
        offset_before, offset_after,
        "navigation must scroll into history"
    );

    ctrl.cancel_pane_search(Some(p1));
    assert!(ctrl.workspace.borrow().pane(p1).unwrap().search().is_none());
    let _ = fs::remove_dir_all(&dir);
}

/// The IPC search is now parked across ticks. Its request and response shapes
/// are unchanged, and the server stays responsive while a scan is in flight.
#[test]
fn ipc_search_is_chunked_and_keeps_the_wire_shape() {
    let server = TestServer::spawn("search", ConsentMode::Paired);
    let sock = server.operator_sock.clone();
    let p1 = server.first_pane;

    send_request(
        &sock,
        &IpcRequest::SendText {
            pane: Some(p1),
            text: String::from("seq 1 12000\n"),
        },
        Duration::from_secs(3),
    )
    .expect("fill scrollback");
    thread::sleep(Duration::from_millis(900));

    let history = match send_request(
        &sock,
        &IpcRequest::Snapshot {
            pane: Some(p1),
            max_lines: Some(1),
        },
        Duration::from_secs(3),
    )
    .expect("snapshot")
    {
        IpcResponse::Ok {
            snapshot: Some(s), ..
        } => s.scrollback_lines,
        other => panic!("unexpected snapshot: {other:?}"),
    };
    assert!(history >= 10_000, "expected full scrollback, got {history}");

    let search_sock = sock.clone();
    let searcher = thread::spawn(move || {
        send_request(
            &search_sock,
            &IpcRequest::Search {
                pane: Some(p1),
                pattern: String::from("absent-needle-xyz"),
                ignore_case: true,
                max_matches: 20,
            },
            Duration::from_secs(10),
        )
        .expect("search reply")
    });

    // While the scan is in flight the tick must still service other requests.
    // The per-tick line budget itself is asserted deterministically in
    // engine::tests::one_chunk_cannot_scan_a_full_history; this checks the
    // server is not blocked, with a generous bound to stay non-flaky.
    for _ in 0..5 {
        let start = std::time::Instant::now();
        let resp = send_request(&sock, &IpcRequest::List, Duration::from_secs(5)).expect("list");
        assert!(matches!(resp, IpcResponse::Ok { .. }));
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "server stalled while a search was in flight: {:?}",
            start.elapsed()
        );
        thread::sleep(Duration::from_millis(10));
    }

    match searcher.join().expect("searcher") {
        IpcResponse::Ok {
            matches: Some(hits),
            pane: Some(pid),
            ..
        } => {
            assert_eq!(pid, p1);
            assert!(
                hits.is_empty(),
                "no-match search must return no hits: {hits:?}"
            );
        }
        other => panic!("unexpected search response: {other:?}"),
    }

    // A hit query still returns matches in the same shape.
    match send_request(
        &sock,
        &IpcRequest::Search {
            pane: Some(p1),
            pattern: String::from("12000"),
            ignore_case: true,
            max_matches: 5,
        },
        Duration::from_secs(10),
    )
    .expect("hit search")
    {
        IpcResponse::Ok {
            matches: Some(hits),
            ..
        } => {
            assert!(!hits.is_empty(), "expected a hit for the last emitted line");
        }
        other => panic!("unexpected hit response: {other:?}"),
    }
}
