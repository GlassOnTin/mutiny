//! The session picker's lifetime and refresh behaviour. Needs GTK, so it skips
//! without a display; CI runs it under Xvfb. One test function because GTK needs
//! a single thread and libtest spawns one per test.
//!
//! Discovery is fed by a fake tmux that prints a fixture, so this test does not
//! depend on tmux being installed and never touches a real session.

use std::rc::Rc;

use gtk4::prelude::*;
use mutiny::app::AppController;
use mutiny::config::Overrides;

fn settle(ctrl: &Rc<AppController>, rounds: usize) {
    for _ in 0..rounds {
        ctrl.poll_once();
        while gtk4::glib::MainContext::default().iteration(false) {}
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Pumps until discovery finishes, bounded so a hang fails the test.
fn settle_discovery(ctrl: &Rc<AppController>) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < end {
        settle(ctrl, 2);
        if !ctrl.discovery_in_flight() {
            settle(ctrl, 3);
            return;
        }
    }
    panic!("discovery did not finish");
}

#[test]
fn picker_survives_a_topology_rebuild_and_dies_with_its_window() {
    if gtk4::init().is_err() {
        eprintln!("skipping: no display (CI runs under Xvfb)");
        return;
    }
    let dir = std::env::temp_dir().join(format!("mutiny-picker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A fake tmux emitting the real TAB-separated listing format.
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
            "[sessions]\ntmux_program = {:?}\nzellij_program = \"/nonexistent-zellij\"\nssh_program = \"/nonexistent-ssh\"\n",
            fake.display().to_string()
        ),
    )
    .unwrap();

    let app = gtk4::Application::new(
        Some("dev.mutiny.picker.test"),
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
    let (w, _t, _p) = ctrl.create_window(None).expect("window");
    while gtk4::glib::MainContext::default().iteration(false) {}
    // Use the controller's own view instance, and open the picker through the
    // registered action, exactly as Ctrl+Shift+S does. A separately constructed
    // MutinyWindow would not be the one the tick refreshes.
    let window = ctrl.ui_window(w).expect("controller owns the view");
    window.rebuild_from_model();

    assert_eq!(window.session_picker_rows(), None, "no picker yet");
    ctrl.execute_action(mutiny::action::ActionId::SessionPicker, Some(w));
    settle_discovery(&ctrl);

    let rows = window.session_picker_rows().expect("picker is open");
    assert!(
        rows >= 2,
        "both fixture sessions must be listed, got {rows}"
    );
    let selected = window.session_picker_selected().expect("a row is selected");
    assert_eq!(selected, "alpha", "first live session selected by default");

    // A topology change tears down every pane widget. The picker is a separate
    // top-level, so it must survive with its selection intact.
    ctrl.create_tab(Some(w), None).expect("new tab");
    settle(&ctrl, 10);
    assert_eq!(
        window
            .session_picker_rows()
            .expect("picker survived the rebuild"),
        rows,
        "a rebuild must not drop the picker"
    );
    assert_eq!(
        window.session_picker_selected().as_deref(),
        Some("alpha"),
        "selection must survive by identity, not row index"
    );

    // A refresh with new results must not move the highlight either.
    std::fs::write(
        &fake,
        "#!/bin/sh\nprintf 'alpha\\t1\\t0\\t1\\t/home/x\\nbeta\\t2\\t1\\t2\\t/home/y\\ngamma\\t1\\t0\\t3\\t/home/z\\n'\n",
    )
    .unwrap();
    let targets = ctrl.discovery_targets_all();
    ctrl.begin_discovery(targets, true, None);
    settle_discovery(&ctrl);
    assert!(
        window.session_picker_rows().expect("picker still open") > rows,
        "the new session must appear"
    );
    assert_eq!(
        window.session_picker_selected().as_deref(),
        Some("alpha"),
        "results arriving mid-refresh must not move the selection"
    );

    // Closing the parent must take the picker with it: GTK keeps constructed
    // windows alive in a global toplevel list, so without destroy_with_parent a
    // stale picker could open sessions into a dead window.
    window.close_programmatically();
    settle(&ctrl, 20);
    assert_eq!(
        window.session_picker_rows(),
        None,
        "the picker must not outlive its window"
    );

    ctrl.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}
