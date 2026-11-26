//! The Sessions submenu must be populated on its own, not only after the user
//! opens the picker window. Ian's bug: it was empty on first load.
//!
//! Needs GTK; skips without a display. One test function because GTK needs a
//! single thread and libtest spawns one per test.

use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::gio::prelude::*;
use mutiny::app::AppController;
use mutiny::config::Overrides;

fn pump(ctrl: &Rc<AppController>, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        ctrl.poll_once();
        while gtk4::glib::MainContext::default().iteration(false) {}
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn labels(menu: &gtk4::gio::Menu) -> Vec<String> {
    (0..menu.n_items())
        .map(|i| {
            menu.item_attribute_value(i, "label", None)
                .and_then(|v| v.get::<String>())
                .unwrap_or_default()
        })
        .collect()
}

#[test]
fn sessions_submenu_fills_without_opening_the_picker() {
    if gtk4::init().is_err() {
        eprintln!("skipping: no display (CI runs under Xvfb)");
        return;
    }
    let dir = std::env::temp_dir().join(format!("mutiny-sessmenu-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let fake = dir.join("fake-tmux");
    std::fs::write(
        &fake,
        "#!/bin/sh\nprintf 'alpha\\t1\\t0\\t1\\t/home/x\\nbeta\\t2\\t1\\t2\\t/home/y\\n'\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[sessions]\ntmux_program = {:?}\nzellij_program = \"/nonexistent-zellij\"\nssh_program = \"/nonexistent-ssh\"\nrefresh_interval_s = 1\n",
            fake.display().to_string()
        ),
    )
    .unwrap();

    let app = gtk4::Application::new(
        Some("dev.mutiny.sessmenu.test"),
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
    ctrl.attach_gtk_app(&app);

    let (root, sessions) = mutiny::action::build_menubar_model();
    assert_eq!(root.n_items(), 5);
    ctrl.register_sessions_menu(sessions.clone());
    assert_eq!(
        labels(&sessions),
        vec!["No sessions found", "Browse All Sessions…"],
        "before any scan the submenu says so and still offers the picker"
    );

    let (_w, _t, _p) = ctrl.create_window(None).expect("window");
    // No picker is opened. The tick alone must fill the submenu.
    pump(&ctrl, 1500);
    let got = labels(&sessions);
    assert!(
        got.iter().any(|l| l == "alpha") && got.iter().any(|l| l == "beta"),
        "the submenu must populate without opening the picker: {got:?}"
    );
    assert_eq!(got.last().unwrap(), "Browse All Sessions…");

    // A refresh must not blank the list while it runs: the previous sessions
    // stay until each target's result replaces them.
    std::fs::write(
        &fake,
        "#!/bin/sh\nsleep 0.4\nprintf 'gamma\\t1\\t0\\t3\\t/home/z\\n'\n",
    )
    .unwrap();
    let targets = ctrl.discovery_targets_all();
    ctrl.begin_discovery(targets, true, None);
    // The inventory itself must keep known sessions while a pass runs, not
    // discard them and start from empty.
    assert!(
        !ctrl.session_inventory().sessions.is_empty(),
        "a refresh must not discard the sessions it already knows"
    );
    let mid = labels(&sessions);
    assert!(
        mid.iter().any(|l| l == "alpha"),
        "a refresh must not flash an empty list: {mid:?}"
    );
    pump(&ctrl, 2000);
    let after = labels(&sessions);
    assert!(
        after.iter().any(|l| l == "gamma"),
        "the refreshed session must appear: {after:?}"
    );
    assert!(
        !after.iter().any(|l| l == "alpha"),
        "a session that disappeared must be dropped: {after:?}"
    );

    // Closing windows must prune their registered Sessions submenus rather than
    // retaining dead menu models across the session lifetime.
    assert_eq!(
        ctrl.sessions_menu_count(),
        2,
        "app menu + first window menu"
    );
    let (w2, _t2, p2) = ctrl.create_window(None).expect("second window");
    assert_eq!(ctrl.sessions_menu_count(), 3);
    assert!(ctrl.close_window(w2));
    assert_eq!(
        ctrl.sessions_menu_count(),
        2,
        "close_window must prune that window's submenu"
    );
    let (_w3, _t3, p3) = ctrl.create_window(None).expect("third window");
    assert_eq!(ctrl.sessions_menu_count(), 3);
    let _ = p2;
    ctrl.close_pane(p3);
    assert_eq!(
        ctrl.sessions_menu_count(),
        2,
        "closing the last pane of a window must also prune its submenu"
    );

    ctrl.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}
