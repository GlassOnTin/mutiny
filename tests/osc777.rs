//! OSC 777 notifications: parsing, forwarding, and the tab badge. The badge
//! assertion needs GTK and skips without a display; CI runs it under Xvfb.

use std::cell::RefCell;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use mutiny::app::AppController;
use mutiny::config::Overrides;
use mutiny::engine::{AttentionReason, EngineNotification, PaneEngine};
use mutiny::notify::DesktopNotifier;

#[derive(Default)]
struct Recorder {
    sent: RefCell<Vec<(String, String, String)>>,
}

impl DesktopNotifier for Recorder {
    fn notify(&self, app_name: &str, title: &str, body: &str) {
        self.sent
            .borrow_mut()
            .push((app_name.to_owned(), title.to_owned(), body.to_owned()));
    }
}

fn notifications(engine: &mut PaneEngine, bytes: &[u8]) -> Vec<(String, String)> {
    engine
        .advance_bytes(bytes)
        .into_iter()
        .filter_map(|n| match n {
            EngineNotification::DesktopNotification { title, body } => Some((title, body)),
            _ => None,
        })
        .collect()
}

#[test]
fn osc777_notify_parses_title_and_body_and_raises_attention() {
    let mut eng = PaneEngine::new_headless(80, 24);
    let got = notifications(&mut eng, b"\x1b]777;notify;Build done;exit 0\x07");
    assert_eq!(got, vec![("Build done".into(), "exit 0".into())]);
    assert_eq!(eng.attention(), Some(AttentionReason::OscNotification));
}

#[test]
fn osc777_title_only_and_st_terminator_and_chunked() {
    let mut eng = PaneEngine::new_headless(80, 24);
    assert_eq!(
        notifications(&mut eng, b"\x1b]777;notify;Only a title\x07"),
        vec![("Only a title".into(), String::new())]
    );

    // ST terminator instead of BEL.
    let mut eng = PaneEngine::new_headless(80, 24);
    assert_eq!(
        notifications(&mut eng, b"\x1b]777;notify;T;B\x1b\\"),
        vec![("T".into(), "B".into())]
    );

    // Split across reads, as a PTY delivers it.
    let mut eng = PaneEngine::new_headless(80, 24);
    assert!(notifications(&mut eng, b"\x1b]777;notify;Chun").is_empty());
    assert_eq!(
        notifications(&mut eng, b"ked;body\x07"),
        vec![("Chunked".into(), "body".into())]
    );
}

#[test]
fn osc777_empty_title_and_other_subcommands_are_ignored() {
    let mut eng = PaneEngine::new_headless(80, 24);
    assert!(notifications(&mut eng, b"\x1b]777;notify;\x07").is_empty());
    assert!(notifications(&mut eng, b"\x1b]777;notify;   ;body\x07").is_empty());
    assert!(notifications(&mut eng, b"\x1b]777;somethingelse;x\x07").is_empty());
    assert_eq!(eng.attention(), None, "no attention without a notification");
}

#[test]
fn controller_forwards_to_the_desktop_notifier_and_badges_the_pane() {
    let dir = std::env::temp_dir().join(format!("mutiny-osc777-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        Overrides::default(),
    );
    let recorder = Rc::new(Recorder::default());
    ctrl.set_notifier(recorder.clone());
    let (_w, _t, pid) = ctrl.create_window(None).expect("pane");

    // Let the shell finish starting before typing, as a human would.
    let warm = Instant::now() + Duration::from_millis(600);
    while Instant::now() < warm {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(10));
    }

    // Emit through the real PTY so the sequence arrives via drain_pty, which
    // is the path poll_once forwards from. Injecting with advance_bytes would
    // set attention but bypass the notification pipeline entirely.
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .write_to_pty(b"printf '\\033]777;notify;Deploy finished;all green\\007'\n")
        .expect("write");

    let end = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < end {
        ctrl.poll_once();
        if !recorder.sent.borrow().is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        recorder.sent.borrow().as_slice(),
        &[(
            "mutiny".into(),
            "Deploy finished".into(),
            "all green".into()
        )],
        "the OSC 777 payload must reach the desktop notifier"
    );
    assert_eq!(
        ctrl.workspace.borrow().pane(pid).unwrap().attention(),
        Some(AttentionReason::OscNotification),
        "the notifying pane must carry attention, which is what badges the tab"
    );

    // Focusing the pane clears the badge, like bell and agent notices.
    ctrl.workspace.borrow_mut().focus_pane(pid).unwrap();
    assert_eq!(ctrl.workspace.borrow().pane(pid).unwrap().attention(), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The tab badge itself, rendered by the window chrome. One GTK test function
/// because libtest spawns a thread per test and GTK needs a single thread.
#[test]
fn notification_sets_the_tab_badge() {
    if gtk4::init().is_err() {
        eprintln!("skipping badge assertion: no display (CI runs under Xvfb)");
        return;
    }
    use gtk4::prelude::*;

    let dir = std::env::temp_dir().join(format!("mutiny-osc777-badge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let app = gtk4::Application::new(
        Some("dev.mutiny.osc777.test"),
        gtk4::gio::ApplicationFlags::NON_UNIQUE,
    );
    app.register(gtk4::gio::Cancellable::NONE)
        .expect("register");

    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        Overrides::default(),
    );
    ctrl.set_notifier(Rc::new(Recorder::default()));
    ctrl.attach_gtk_app(&app);
    let (w, _t, pid) = ctrl.create_window(None).expect("window");
    while gtk4::glib::MainContext::default().iteration(false) {}

    let window = mutiny::ui::window::MutinyWindow::new(&app, w, Rc::clone(&ctrl));
    window.rebuild_from_model();
    let before = window.tab_labels();
    assert!(
        !before.iter().any(|l| l.contains("!notify")),
        "no badge before a notification: {before:?}"
    );

    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .advance_bytes(b"\x1b]777;notify;Build finished;ok\x07");
    ctrl.poll_once();
    window.refresh_tab_strip_labels();
    let after = window.tab_labels();
    assert!(
        after.iter().any(|l| l.contains("!notify")),
        "the notifying tab must be badged: {after:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
