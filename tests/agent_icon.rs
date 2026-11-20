//! The app icon must signal agent control: idle skull normally, blue with red
//! eyes while an external agent is driving, back to idle when it goes quiet.
//!
//! Needs GTK; skips without a display. One test function because GTK needs a
//! single thread and libtest spawns one per test.

use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use mutiny::app::AppController;
use mutiny::config::Overrides;
use mutiny::desktop::{AGENT_ICON_NAME, APP_ID};
use mutiny::ipc::{ConsentMode, IpcRequest, send_request};

fn settle(ctrl: &Rc<AppController>, rounds: usize) {
    for _ in 0..rounds {
        ctrl.poll_once();
        while gtk4::glib::MainContext::default().iteration(false) {}
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn icon_lights_up_while_an_agent_drives_and_reverts_when_idle() {
    if gtk4::init().is_err() {
        eprintln!("skipping: no display (CI runs under Xvfb)");
        return;
    }
    let dir = std::env::temp_dir().join(format!("mutiny-agenticon-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Both icon states must resolve through GTK, not just exist on disk.
    let display = gtk4::gdk::Display::default().expect("display");
    let theme = gtk4::IconTheme::for_display(&display);
    let path = mutiny::desktop::ensure_icon_search_path().expect("stage icons");
    theme.add_search_path(&path);
    for name in [APP_ID, AGENT_ICON_NAME] {
        let paintable = theme.lookup_icon(
            name,
            &[],
            16,
            1,
            gtk4::TextDirection::None,
            gtk4::IconLookupFlags::empty(),
        );
        assert!(
            paintable.file().and_then(|f| f.path()).is_some(),
            "GTK could not resolve {name}"
        );
    }

    let app = gtk4::Application::new(
        Some("dev.mutiny.agenticon.test"),
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
    let agent_sock = dir.join("agent.sock");
    ctrl.start_agent_listener(&agent_sock, ConsentMode::Paired)
        .expect("agent listener");
    ctrl.attach_gtk_app(&app);
    let (w, _t, _p) = ctrl.create_window(None).expect("window");
    settle(&ctrl, 10);
    let window = ctrl.ui_window(w).expect("view");

    // Idle: the normal skull.
    assert!(!ctrl.agent_active(), "no agent has acted yet");
    assert_eq!(window.icon_name().as_deref(), Some(APP_ID));

    // An agent call on the agent carrier lights it up. The controller only
    // accepts connections inside poll_once, so the request runs on its own
    // thread while the main loop is pumped, or it would deadlock.
    let sock = agent_sock.clone();
    let caller =
        std::thread::spawn(move || send_request(&sock, &IpcRequest::List, Duration::from_secs(10)));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !caller.is_finished() && std::time::Instant::now() < deadline {
        settle(&ctrl, 2);
    }
    caller.join().expect("caller").expect("agent call");
    settle(&ctrl, 6);
    assert!(ctrl.agent_active(), "the agent call must register");
    assert_eq!(
        window.icon_name().as_deref(),
        Some(AGENT_ICON_NAME),
        "the icon must switch to the agent-active variant"
    );
    // The icon and the in-window badge are the same signal, so they agree.
    assert_eq!(ctrl.agent_active(), ctrl.active_agent_badge().is_some());

    // A window opened mid-session inherits the active state immediately.
    let (w2, _t2, _p2) = ctrl.create_window(None).expect("second window");
    settle(&ctrl, 6);
    let window2 = ctrl.ui_window(w2).expect("second view");
    assert_eq!(
        window2.icon_name().as_deref(),
        Some(AGENT_ICON_NAME),
        "a new window must not start idle while an agent is driving"
    );

    // The 15 s freshness window is the debounce: a brief act holds the state
    // rather than flickering. Still active well after the call returned.
    settle(&ctrl, 20);
    assert!(ctrl.agent_active(), "a brief act must not flicker off");
    assert_eq!(window.icon_name().as_deref(), Some(AGENT_ICON_NAME));

    // Expiring the window reverts every window to the idle icon.
    ctrl.clear_recent_agent_for_test();
    settle(&ctrl, 6);
    assert!(!ctrl.agent_active());
    assert_eq!(window.icon_name().as_deref(), Some(APP_ID));
    assert_eq!(window2.icon_name().as_deref(), Some(APP_ID));

    ctrl.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}
