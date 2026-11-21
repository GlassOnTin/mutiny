//! Mouse reporting: when a program asks for mouse events, the wheel and clicks
//! belong to it, not to mutiny's scrollback and selection.
//!
//! Ian's bug: scrolling in tmux did nothing, because the wheel handler always
//! scrolled mutiny's own scrollback and never forwarded to the PTY.

use mutiny::engine::{
    MOUSE_MOTION_FLAG, MOUSE_WHEEL_DOWN, MOUSE_WHEEL_UP, WheelAction, mouse_report, wheel_action,
};

#[test]
fn wheel_goes_to_the_program_in_mouse_mode_and_to_scrollback_otherwise() {
    // tmux with mouse on: the wheel must be reported, not swallowed.
    assert_eq!(
        wheel_action(true, false, -1.0),
        WheelAction::Report(MOUSE_WHEEL_UP)
    );
    assert_eq!(
        wheel_action(true, false, 1.0),
        WheelAction::Report(MOUSE_WHEEL_DOWN)
    );
    // No mouse mode: local scrollback, as before.
    assert_eq!(
        wheel_action(false, false, -1.0),
        WheelAction::LocalScroll(3)
    );
    assert_eq!(
        wheel_action(false, false, 1.0),
        WheelAction::LocalScroll(-3)
    );
    // Shift is the override back to local scrollback even in mouse mode.
    assert_eq!(wheel_action(true, true, -1.0), WheelAction::LocalScroll(3));
    assert_eq!(wheel_action(true, true, 1.0), WheelAction::LocalScroll(-3));
    assert_eq!(wheel_action(true, false, 0.0), WheelAction::Ignore);
}

#[test]
fn sgr_reports_use_one_based_coordinates_and_press_release_tails() {
    assert_eq!(
        mouse_report(MOUSE_WHEEL_UP, 0, 0, false, true),
        b"\x1b[<64;1;1M".to_vec(),
        "coordinates are 1-based"
    );
    assert_eq!(
        mouse_report(MOUSE_WHEEL_DOWN, 9, 4, false, true),
        b"\x1b[<65;10;5M".to_vec()
    );
    assert_eq!(
        mouse_report(0, 2, 3, false, true),
        b"\x1b[<0;3;4M".to_vec(),
        "press"
    );
    assert_eq!(
        mouse_report(0, 2, 3, true, true),
        b"\x1b[<0;3;4m".to_vec(),
        "release uses a lowercase m"
    );
    assert_eq!(
        mouse_report(MOUSE_MOTION_FLAG, 0, 0, false, true),
        b"\x1b[<32;1;1M".to_vec(),
        "drag motion adds 32"
    );
}

#[test]
fn x10_reports_offset_by_32_and_cap_coordinates() {
    assert_eq!(
        mouse_report(0, 0, 0, false, false),
        vec![0x1b, b'[', b'M', 32, 33, 33]
    );
    assert_eq!(
        mouse_report(0, 0, 0, true, false),
        vec![0x1b, b'[', b'M', 35, 33, 33],
        "X10 encodes any release as button 3"
    );
    // 223 + 32 = 255 is the largest byte the legacy encoding can carry.
    let capped = mouse_report(0, 500, 500, false, false);
    assert_eq!(capped[4], 255);
    assert_eq!(capped[5], 255);
    // SGR has no such limit but we clamp consistently.
    let sgr = String::from_utf8(mouse_report(0, 500, 500, false, true)).unwrap();
    assert!(sgr.contains(";223;223M"), "{sgr}");
}

/// The detection must work against a real program, not just a synthetic flag.
#[test]
fn a_pane_running_tmux_reports_mouse_mode() {
    let tmux = std::env::var("MUTINY_TEST_TMUX")
        .ok()
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| {
            let out = std::process::Command::new("sh")
                .args(["-c", "command -v tmux"])
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if p.is_empty() {
                None
            } else {
                Some(std::path::PathBuf::from(p))
            }
        });
    let Some(tmux) = tmux else {
        eprintln!("skipping: tmux not installed (CI installs it)");
        return;
    };
    let dir = std::env::temp_dir().join(format!("mutiny-mouse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("tmux.sock");
    let run = |args: &[&str]| {
        std::process::Command::new(&tmux)
            .arg("-S")
            .arg(&sock)
            .args(args)
            .output()
            .expect("tmux")
    };
    assert!(run(&["new-session", "-d", "-s", "m"]).status.success());
    assert!(run(&["set", "-g", "mouse", "on"]).status.success());

    let ctrl = mutiny::app::AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        mutiny::config::Overrides::default(),
    );
    let (_w, _t, pid) = ctrl.create_window(None).expect("window");
    {
        let mut ws = ctrl.workspace.borrow_mut();
        let eng = ws.pane_mut(pid).unwrap();
        assert!(
            !eng.mouse_mode(),
            "a plain shell does not ask for the mouse"
        );
    }
    // Attach the pane to the mouse-enabled tmux session.
    let attach = format!("{} -S {} attach -t m\n", tmux.display(), sock.display());
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .write_to_pty(attach.as_bytes())
        .expect("write");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let mut detected = false;
    while std::time::Instant::now() < end {
        ctrl.poll_once();
        if ctrl.workspace.borrow().pane(pid).unwrap().mouse_mode() {
            detected = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        detected,
        "attaching to a mouse-enabled tmux must put the pane in mouse mode"
    );
    // And the wheel decision follows from it.
    let (mouse, sgr) = {
        let ws = ctrl.workspace.borrow();
        let eng = ws.pane(pid).unwrap();
        (eng.mouse_mode(), eng.sgr_mouse())
    };
    assert!(mouse);
    assert_eq!(
        wheel_action(mouse, false, 1.0),
        WheelAction::Report(MOUSE_WHEEL_DOWN)
    );
    eprintln!("tmux mouse mode detected, sgr={sgr}");

    let _ = run(&["kill-server"]);
    ctrl.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}
