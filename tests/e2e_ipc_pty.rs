use std::env;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use mutiny::action::ActionId;
use mutiny::app::AppController;
use mutiny::ipc::{ConsentMode, IpcRequest, IpcResponse, send_request};
use mutiny::mcp::McpBridge;
use mutiny::topology::{PaneId, SplitAxis};
use serde_json::{Value, json};
use std::io::Cursor;

#[test]
fn live_pty_split_osc133_wait_and_detach_over_unix_socket() {
    let sock_path = env::temp_dir().join(format!("mutiny-e2e-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock_path);

    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let server_sock = sock_path.clone();

    let server_thread = thread::spawn(move || {
        let ctrl = AppController::new(Some((String::from("/bin/sh"), Vec::new())));
        ctrl.start_ipc_listener(&server_sock)
            .expect("start IPC listener");
        let (_w, _t, p1) = ctrl.create_window(None).expect("spawn first PTY window");
        ready_tx.send(p1).unwrap();

        while stop_rx.try_recv().is_err() {
            ctrl.poll_once();
            thread::sleep(Duration::from_millis(5));
        }
        ctrl.shutdown_ipc();
    });

    let p1 = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server ready");

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

    // 2. Subscribe to WaitPrompt on a client thread while feeding an OSC 133 command into p2's /bin/sh
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

    // 3. Focus p1, then run UrgentNext over IPC -> must jump straight to p2 (which exited 9)
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

    // 4. Detach p2 into its own top-level window over IPC and verify `List` shows 2 windows
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

    // Verify p2's captured OSC 133 LastOutput survived detaching to a second window
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

    stop_tx.send(()).unwrap();
    server_thread.join().unwrap();
}

#[test]
fn mcp_bridge_drives_agent_turns_resources_consent_and_exited_ring() {
    let sock_path = env::temp_dir().join(format!("mutiny-mcp-e2e-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock_path);

    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let server_sock = sock_path.clone();

    let server_thread = thread::spawn(move || {
        let ctrl = AppController::new(Some((String::from("/bin/sh"), Vec::new())));
        ctrl.start_ipc_listener(&server_sock)
            .expect("start IPC listener");
        let (_w, _t, p1) = ctrl.create_window(None).expect("spawn first PTY window");
        ready_tx.send(p1).unwrap();

        while stop_rx.try_recv().is_err() {
            ctrl.poll_once();
            thread::sleep(Duration::from_millis(5));
        }
        ctrl.shutdown_ipc();
    });

    let p1 = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server ready");

    // 1. Observe-only MCP client can read ui://mutiny/workspace, but Tier-2 writes are denied.
    {
        let mut obs_bridge = McpBridge::new(
            sock_path.clone(),
            ConsentMode::ObserveOnly,
            String::from("observer"),
        );
        let input = format!(
            "{}\n{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"ui://mutiny/workspace"}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"detach_split","arguments":{}}})
        );
        let mut reader = Cursor::new(input.into_bytes());
        let mut out = Vec::new();
        obs_bridge.serve(&mut reader, &mut out).unwrap();
        let replies: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(replies.len(), 2);
        assert!(
            replies[0]["result"]["contents"][0]["text"]
                .as_str()
                .unwrap()
                .contains("\"windows\"")
        );
        assert_eq!(replies[1]["result"]["isError"], json!(true));
    }

    // 2. Paired Haven MCP peer initializes, spawns a split, sends a turn (no OSC 133!), awaits quiescence,
    //    posts an operator notice, then closes that split and inspects `ui://mutiny/exited` & `ui://mutiny/audit`.
    let mut paired = McpBridge::new(
        sock_path.clone(),
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

    // Wait briefly for pane 2's `exit 5` child shell to finish and land in the 8-slot exited ring
    thread::sleep(Duration::from_millis(200));

    let verify_lines = [
        json!({"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"ui://mutiny/exited"}}),
        json!({"jsonrpc":"2.0","id":6,"method":"resources/read","params":{"uri":"ui://mutiny/audit"}}),
        json!({"jsonrpc":"2.0","id":7,"method":"resources/read","params":{"uri":format!("ui://mutiny/pane/{}/snapshot", p1.0)}}),
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
    assert!(audit_json.contains("\"denied\"")); // from observer's rejected detach_split

    let p1_snap = v_replies[2]["result"]["contents"][0]["text"]
        .as_str()
        .unwrap();
    assert!(p1_snap.contains("Build review ready"));

    stop_tx.send(()).unwrap();
    server_thread.join().unwrap();
}
