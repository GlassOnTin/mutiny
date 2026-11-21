//! Session restore end to end: a saved bundle must reproduce geometry, split
//! ratios, working directories, and scrollback in a fresh controller, with
//! fresh shells rather than restored processes.

use std::path::PathBuf;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use mutiny::app::AppController;
use mutiny::config::Overrides;
use mutiny::mux::{MuxKind, SessionMode};
use mutiny::topology::{PaneId, SplitAxis, SplitNode, TabId, WindowId};

fn controller(dir: &std::path::Path, session: &std::path::Path) -> Rc<AppController> {
    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        Overrides::default(),
    );
    ctrl.set_session_path(session.to_path_buf());
    ctrl
}

fn pump(ctrl: &Rc<AppController>, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn restored_bundle_reproduces_geometry_ratios_cwd_and_scrollback() {
    let dir = std::env::temp_dir().join(format!("mutiny-restore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let session_file = dir.join("session.json");

    // --- build a session ---
    let first = controller(&dir, &session_file);
    let (w1, t1, p1) = first.create_window(None).expect("window");
    let p2 = first
        .split_pane(Some(p1), SplitAxis::Horizontal, None)
        .expect("split")
        .2;
    let p3 = first
        .split_pane(Some(p2), SplitAxis::Vertical, None)
        .expect("split2")
        .2;
    assert!(first.set_split_ratio(t1, &[], 0.30));
    assert!(first.set_split_ratio(t1, &[true], 0.65));

    // Give each pane a distinct cwd via OSC 7 and distinct history.
    let cwd = dir.join("workdir");
    std::fs::create_dir_all(&cwd).unwrap();
    for (pid, marker) in [
        (p1, "alpha-history"),
        (p2, "beta-history"),
        (p3, "gamma-history"),
    ] {
        let mut ws = first.workspace.borrow_mut();
        let eng = ws.pane_mut(pid).unwrap();
        eng.advance_bytes(format!("\x1b]7;file://host{}\x07", cwd.display()).as_bytes());
        eng.advance_bytes(format!("{marker} line one\r\n{marker} line two\r\n").as_bytes());
    }
    first.save_session().expect("save");
    assert!(session_file.is_file());

    let original_tree = first.workspace.borrow().window(w1).unwrap().tabs[0]
        .root
        .clone();
    let original_sizes: Vec<(PaneId, usize, usize)> = [p1, p2, p3]
        .iter()
        .map(|pid| {
            let s = first.workspace.borrow().pane(*pid).unwrap().size();
            (*pid, s.cols, s.rows)
        })
        .collect();
    drop(first);

    // --- restore into a fresh controller ---
    let second = controller(&dir, &session_file);
    let restored = second
        .restore_session_from_file()
        .expect("restore")
        .expect("a state file existed");
    assert_eq!(restored, 3, "all three panes must come back");

    assert_eq!(second.workspace.borrow().windows().len(), 1);
    let win = second.workspace.borrow().windows()[0].clone();
    assert_eq!(win.id, w1, "window identity is preserved");
    assert_eq!(win.tabs.len(), 1);
    assert_eq!(win.tabs[0].id, t1);
    assert_eq!(
        win.tabs[0].root, original_tree,
        "tree shape and ratios must match"
    );

    // Ratios explicitly, since they are the point of the rung.
    match &win.tabs[0].root {
        SplitNode::Split { ratio, second, .. } => {
            assert!((ratio - 0.30).abs() < 1e-9, "root ratio, got {ratio}");
            match second.as_ref() {
                SplitNode::Split { ratio: r2, .. } => {
                    assert!((r2 - 0.65).abs() < 1e-9, "nested ratio, got {r2}");
                }
                _ => panic!("expected nested split"),
            }
        }
        _ => panic!("expected a split"),
    }

    // Geometry, cwd, and scrollback per pane.
    for (pid, cols, rows) in &original_sizes {
        let ws = second.workspace.borrow();
        let eng = ws.pane(*pid).expect("pane restored");
        assert_eq!(
            (eng.size().cols, eng.size().rows),
            (*cols, *rows),
            "pane {pid} size"
        );
        assert_eq!(eng.cwd(), Some(cwd.as_path()), "pane {pid} cwd");
    }
    for (pid, marker) in [
        (p1, "alpha-history"),
        (p2, "beta-history"),
        (p3, "gamma-history"),
    ] {
        let text = second
            .workspace
            .borrow()
            .pane(pid)
            .unwrap()
            .scrollback_text()
            .join("\n");
        assert!(
            text.contains(&format!("{marker} line one")),
            "pane {pid} history: {text}"
        );
        assert!(
            text.contains(&format!("{marker} line two")),
            "pane {pid} history: {text}"
        );
    }

    // A restored pane is a FRESH shell, not the old process: it must run a new
    // command. This is the no-daemon contract made visible.
    second
        .workspace
        .borrow_mut()
        .pane_mut(p1)
        .unwrap()
        .write_to_pty(b"echo restored-shell-alive\n")
        .expect("write");
    pump(&second, 900);
    let screen = second.workspace.borrow().pane(p1).unwrap().screen_text();
    assert!(
        screen.contains("restored-shell-alive"),
        "a restored pane must run a fresh shell; screen was:\n{screen}"
    );

    // Restoring again from the same file is idempotent in shape.
    second.save_session().expect("re-save");
    let third = controller(&dir, &session_file);
    assert_eq!(third.restore_session_from_file().unwrap(), Some(3));
    assert_eq!(
        third.workspace.borrow().windows()[0].tabs[0].root,
        original_tree
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_state_file_restores_nothing_and_corrupt_state_is_reported() {
    let dir = std::env::temp_dir().join(format!("mutiny-restore-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let session_file = dir.join("session.json");
    let ctrl = controller(&dir, &session_file);
    assert_eq!(ctrl.restore_session_from_file().unwrap(), None);
    assert!(ctrl.workspace.borrow().windows().is_empty());

    std::fs::write(&session_file, "{ corrupt").unwrap();
    assert!(ctrl.restore_session_from_file().is_err());
    assert!(
        ctrl.workspace.borrow().windows().is_empty(),
        "a corrupt file must not half-restore"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The saved bundle must carry the opened session list with its window and tab
/// ordering, and re-attach those sessions on the next load rather than coming
/// back as bare shells.
#[test]
fn saved_session_list_reopens_in_order_and_reattaches() {
    let tmux = std::env::var("MUTINY_TEST_TMUX")
        .ok()
        .map(PathBuf::from)
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
                Some(PathBuf::from(p))
            }
        });
    let Some(tmux) = tmux else {
        eprintln!("skipping: tmux not installed (CI installs it)");
        return;
    };
    let dir = std::env::temp_dir().join(format!("mutiny-relist-{}", std::process::id()));
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
    for name in ["alpha", "beta", "gamma"] {
        assert!(run(&["new-session", "-d", "-s", name]).status.success());
    }
    let session_file = dir.join("session.json");
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[sessions]\ntmux_program = {:?}\ntmux_socket = {:?}\nzellij_program = \"/nonexistent-zellij\"\nssh_program = \"/nonexistent-ssh\"\n",
            tmux.display().to_string(), sock.display().to_string()
        ),
    ).unwrap();
    let make = || {
        let c = AppController::with_config(
            Some((String::from("/bin/sh"), Vec::new())),
            dir.join("audit.jsonl"),
            dir.join("config.toml"),
            Overrides::default(),
        );
        c.set_session_path(session_file.clone());
        c
    };

    // Window 1: tabs alpha, beta. Window 2: tab gamma plus a plain shell.
    let first = make();
    let (w1, t1, p1) = first.create_window(None).expect("w1");
    first
        .open_session(
            MuxKind::Tmux,
            "alpha",
            None,
            SessionMode::Tab,
            Some(w1),
            None,
        )
        .expect("alpha");
    let (_w, t2, p_alpha) = first
        .open_session(
            MuxKind::Tmux,
            "beta",
            None,
            SessionMode::Tab,
            Some(w1),
            None,
        )
        .expect("beta");
    let (w2, t3, _p) = first.create_window(None).expect("w2");
    let (_w2, _t4, p_gamma) = first
        .open_session(
            MuxKind::Tmux,
            "gamma",
            None,
            SessionMode::Tab,
            Some(w2),
            None,
        )
        .expect("gamma");
    let (_w2b, t_plain, p_plain) = first.create_tab(Some(w2), None).expect("plain tab");
    first
        .workspace
        .borrow_mut()
        .pane_mut(p_plain)
        .unwrap()
        .advance_bytes(b"plain-shell-history\r\n");
    pump(&first, 1200);

    let expected_windows: Vec<WindowId> = first
        .workspace
        .borrow()
        .windows()
        .iter()
        .map(|w| w.id)
        .collect();
    let expected_tabs: Vec<Vec<TabId>> = first
        .workspace
        .borrow()
        .windows()
        .iter()
        .map(|w| w.tabs.iter().map(|t| t.id).collect())
        .collect();
    let expected_names: Vec<Vec<Option<String>>> = first
        .workspace
        .borrow()
        .windows()
        .iter()
        .map(|w| {
            w.tabs
                .iter()
                .map(|t| {
                    first
                        .workspace
                        .borrow()
                        .pane(t.active_pane)
                        .and_then(|e| e.mux_session())
                        .map(|s| s.name.clone())
                })
                .collect()
        })
        .collect();
    first.save_session().expect("save");
    let _ = (t1, t2, t3, t_plain, p1, p_alpha, p_gamma);
    drop(first);

    // A fresh controller reopens the list.
    let second = make();
    let restored = second
        .restore_session_from_file()
        .expect("restore")
        .expect("bundle");
    assert_eq!(restored, 6, "all six panes come back");
    let got_windows: Vec<WindowId> = second
        .workspace
        .borrow()
        .windows()
        .iter()
        .map(|w| w.id)
        .collect();
    assert_eq!(
        got_windows, expected_windows,
        "window order must be preserved"
    );
    let got_tabs: Vec<Vec<TabId>> = second
        .workspace
        .borrow()
        .windows()
        .iter()
        .map(|w| w.tabs.iter().map(|t| t.id).collect())
        .collect();
    assert_eq!(got_tabs, expected_tabs, "tab order must be preserved");
    let got_names: Vec<Vec<Option<String>>> = second
        .workspace
        .borrow()
        .windows()
        .iter()
        .map(|w| {
            w.tabs
                .iter()
                .map(|t| {
                    second
                        .workspace
                        .borrow()
                        .pane(t.active_pane)
                        .and_then(|e| e.mux_session())
                        .map(|s| s.name.clone())
                })
                .collect()
        })
        .collect();
    assert_eq!(
        got_names, expected_names,
        "the session list must match, in order"
    );
    assert_eq!(
        got_names,
        vec![
            vec![None, Some("alpha".into()), Some("beta".into())],
            vec![None, Some("gamma".into()), None]
        ],
        "sanity: the arrangement we built"
    );

    // The attached panes must really re-attach, not just carry a label.
    pump(&second, 2000);
    let clients = run(&["list-clients", "-F", "#{client_session}"]);
    let text = String::from_utf8_lossy(&clients.stdout);
    for name in ["alpha", "beta", "gamma"] {
        assert!(
            text.lines().any(|l| l == name),
            "restored pane must re-attach to {name}; clients:\n{text}"
        );
    }

    // A plain pane still replays its history; an attached one must not, because
    // the multiplexer owns and redraws that screen.
    let plain_text = second
        .workspace
        .borrow()
        .pane(p_plain)
        .unwrap()
        .scrollback_text()
        .join("\n");
    assert!(
        plain_text.contains("plain-shell-history"),
        "plain pane history: {plain_text}"
    );

    let _ = run(&["kill-server"]);
    second.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}
