//! Restoring must produce real windows, not just a populated model.
//!
//! The bug this guards: restore ran before `attach_gtk_app`, so
//! `ensure_ui_window` returned early and created nothing. The model had windows,
//! the application had none, and GTK exited immediately. Session restore had
//! never actually worked in the GUI; the headless tests could not see it.
//!
//! Needs GTK; skips without a display. One test function because GTK needs a
//! single thread.

use std::rc::Rc;

use gtk4::prelude::*;
use mutiny::app::AppController;
use mutiny::config::Overrides;

#[test]
fn a_restore_before_the_app_is_attached_still_produces_windows() {
    if gtk4::init().is_err() {
        eprintln!("skipping: no display (CI runs under Xvfb)");
        return;
    }
    let dir = std::env::temp_dir().join(format!("mutiny-restoregui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let session_file = dir.join("session.json");
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

    // Two windows, three tabs, saved headlessly.
    let first = make();
    let (w1, _t, _p) = first.create_window(None).expect("w1");
    first.create_tab(Some(w1), None).expect("tab");
    let (_w2, _t2, _p2) = first.create_window(None).expect("w2");
    first.save_session().expect("save");
    drop(first);

    let app = gtk4::Application::new(
        Some("dev.mutiny.restoregui.test"),
        gtk4::gio::ApplicationFlags::NON_UNIQUE,
    );
    app.register(gtk4::gio::Cancellable::NONE)
        .expect("register");

    // Deliberately restore BEFORE attaching, which is the ordering that used to
    // lose every window.
    let second: Rc<AppController> = make();
    let restored = second
        .restore_session_from_file()
        .expect("restore")
        .expect("bundle");
    assert_eq!(restored, 3);
    assert!(
        second.ui_window(w1).is_none(),
        "no view can exist before the application is attached"
    );

    second.attach_gtk_app(&app);
    while gtk4::glib::MainContext::default().iteration(false) {}

    let ids: Vec<mutiny::topology::WindowId> = second
        .workspace
        .borrow()
        .windows()
        .iter()
        .map(|w| w.id)
        .collect();
    assert_eq!(ids.len(), 2, "both windows are in the model");
    for id in ids {
        assert!(
            second.ui_window(id).is_some(),
            "window {} must have a real view after attaching, or GTK exits with nothing to show",
            id.0
        );
    }

    second.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}
