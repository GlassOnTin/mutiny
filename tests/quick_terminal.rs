//! The quick terminal is a scratch shell you summon and dismiss. Its whole
//! promise is that dismissing it does NOT end the shell, so the assertions here
//! are about survival and identity, not about a window appearing.
//!
//! A toggle that destroyed the window on dismiss would still look right in a
//! screenshot. These tests fail on that: the pane id must be the same after a
//! round trip, and output produced while dismissed must still be there.

use std::rc::Rc;

use mutiny::app::AppController;
use mutiny::config::Overrides;

fn settle(ctrl: &Rc<AppController>, rounds: usize) {
    for _ in 0..rounds {
        ctrl.poll_once();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("mutiny-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A config that prefers tmux, so the test can prove the scratch pane ignores
/// that preference and stays a plain shell.
fn write_config(dir: &std::path::Path) {
    std::fs::write(
        dir.join("config.toml"),
        concat!(
            "[sessions]\n",
            "tmux_program = \"/nonexistent-tmux\"\n",
            "zellij_program = \"/nonexistent-zellij\"\n",
            "ssh_program = \"/nonexistent-ssh\"\n",
            "preferred = \"tmux\"\n",
            "refresh_interval_s = 0\n",
        ),
    )
    .unwrap();
}

fn controller(dir: &std::path::Path) -> Rc<AppController> {
    write_config(dir);
    AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        Overrides::default(),
    )
}

#[test]
fn dismissing_keeps_the_shell_and_resummoning_reuses_the_same_pane() {
    let dir = scratch_dir("quick");
    let ctrl = controller(&dir);
    ctrl.set_session_path(dir.join("session.json"));

    assert!(!ctrl.quick_visible(), "nothing is summoned to begin with");
    let (win, first_show) = ctrl.toggle_quick_terminal().expect("first summon");
    assert!(first_show, "the first toggle must summon, not dismiss");
    assert!(ctrl.quick_visible());
    let pane = ctrl
        .workspace
        .borrow()
        .window(win)
        .and_then(|w| w.active_pane())
        .expect("a pane");
    assert!(
        ctrl.workspace.borrow().window(win).unwrap().quick,
        "the window must be marked quick so a WM close dismisses it"
    );
    let summary = ctrl.workspace.borrow().summary();
    assert!(
        summary.windows.iter().any(|w| w.quick && w.id == win),
        "ctl list must show which window is the quick one, or an agent cannot tell"
    );
    assert!(
        ctrl.workspace
            .borrow()
            .pane(pane)
            .unwrap()
            .mux_session()
            .is_none(),
        "a scratch pane must stay a plain shell even when tmux is preferred"
    );
    settle(&ctrl, 20);

    // Produce output BEFORE dismissing.
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pane)
        .unwrap()
        .write_to_pty(b"echo QUICKBEFORE\n")
        .unwrap();
    settle(&ctrl, 30);

    let (_, second_show) = ctrl.toggle_quick_terminal().expect("dismiss");
    assert!(!second_show, "the second toggle must dismiss");
    assert!(
        ctrl.workspace.borrow().pane(pane).is_some(),
        "dismissing must not drop the engine"
    );
    assert!(
        ctrl.workspace.borrow().window(win).is_some(),
        "dismissing must not remove the window from the model"
    );

    // The shell keeps running while dismissed: this is the point of the verb.
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pane)
        .unwrap()
        .write_to_pty(b"echo QUICKWHILEHIDDEN\n")
        .unwrap();
    settle(&ctrl, 30);

    let (again, third_show) = ctrl.toggle_quick_terminal().expect("re-summon");
    assert!(third_show);
    assert_eq!(
        again, win,
        "re-summoning must reuse the window, not spawn one"
    );
    assert_eq!(
        ctrl.workspace
            .borrow()
            .window(again)
            .and_then(|w| w.active_pane()),
        Some(pane),
        "re-summoning must reuse the pane, so the shell and its history survive"
    );
    let text = ctrl.workspace.borrow().pane(pane).unwrap().screen_text();
    assert!(
        text.contains("QUICKBEFORE"),
        "output from before the dismiss must survive, got {text:?}"
    );
    assert!(
        text.contains("QUICKWHILEHIDDEN"),
        "the shell must have run the command issued while dismissed, got {text:?}"
    );
    assert_eq!(
        ctrl.workspace.borrow().windows().len(),
        1,
        "three toggles must leave exactly one window"
    );

    // The mark travels with the saved layout, and a restore brings the quick
    // window back dismissed rather than putting it in the user's face.
    ctrl.save_session().expect("save");
    let state = mutiny::session::load(&dir.join("session.json"))
        .expect("load")
        .expect("a bundle");
    assert!(
        state.windows.iter().any(|w| w.quick),
        "the quick mark must persist"
    );
    let restored = controller(&dir);
    restored.set_session_path(dir.join("session.json"));
    restored.restore_session(state).expect("restore");
    assert!(
        !restored.quick_visible(),
        "a restored quick terminal starts dismissed"
    );
    let (_, show) = restored
        .toggle_quick_terminal()
        .expect("summon after restore");
    assert!(show, "the restored window must be the one summoned");
    assert_eq!(restored.workspace.borrow().windows().len(), 1);

    ctrl.shutdown_ipc();
    restored.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_explicit_close_ends_the_quick_window_and_the_next_toggle_makes_a_new_one() {
    let dir = scratch_dir("quickclose");
    let ctrl = controller(&dir);
    ctrl.set_session_path(dir.join("session.json"));
    let (win, _) = ctrl.toggle_quick_terminal().unwrap();
    settle(&ctrl, 10);
    // Close Window is explicit destruction, unlike a WM close, so it really
    // ends the scratch shell and the next summon starts fresh.
    assert!(ctrl.close_window(win));
    assert!(!ctrl.quick_visible());
    assert!(ctrl.workspace.borrow().window(win).is_none());
    let (win2, show) = ctrl.toggle_quick_terminal().unwrap();
    assert!(show);
    assert_ne!(
        win2, win,
        "a destroyed quick window must not be resurrected"
    );
    ctrl.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The GTK half: the window really hides and shows, and a WM close dismisses it
/// instead of destroying the pane. Needs a display; CI runs this under Xvfb.
#[test]
fn the_quick_window_really_hides_and_a_wm_close_only_dismisses_it() {
    if gtk4::init().is_err() {
        eprintln!("skipping: no display (CI runs under Xvfb)");
        return;
    }
    use gtk4::prelude::*;

    let dir = scratch_dir("quickgtk");
    let app = gtk4::Application::new(
        Some("dev.mutiny.quick.test"),
        gtk4::gio::ApplicationFlags::NON_UNIQUE,
    );
    app.register(gtk4::gio::Cancellable::NONE)
        .expect("register");
    write_config(&dir);
    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        Overrides::default(),
    );
    ctrl.set_session_path(dir.join("session.json"));
    ctrl.attach_gtk_app(&app);
    let settle_gtk = |rounds: usize| {
        for _ in 0..rounds {
            ctrl.poll_once();
            while gtk4::glib::MainContext::default().iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };

    let (win, _) = ctrl.toggle_quick_terminal().expect("summon");
    settle_gtk(15);
    let ui = ctrl.ui_window(win).expect("a view");
    let gtk_win = ui.gtk_window();
    assert!(gtk_win.is_visible(), "summoning must show the window");
    assert!(
        gtk_win.hides_on_close(),
        "a WM close must hide it, or the scratch shell dies with the window"
    );
    let pane = ctrl
        .workspace
        .borrow()
        .window(win)
        .and_then(|w| w.active_pane())
        .unwrap();

    ctrl.toggle_quick_terminal().expect("dismiss");
    settle_gtk(15);
    assert!(
        !gtk_win.is_visible(),
        "dismissing must hide the real window"
    );
    assert!(ctrl.workspace.borrow().pane(pane).is_some());

    ctrl.toggle_quick_terminal().expect("re-summon");
    settle_gtk(15);
    assert!(gtk_win.is_visible(), "re-summoning must show it again");

    // A WM close is a dismiss, not a destruction.
    gtk_win.close();
    settle_gtk(15);
    assert!(
        ctrl.workspace.borrow().window(win).is_some(),
        "a WM close must leave the quick window in the model"
    );
    assert!(
        ctrl.workspace.borrow().pane(pane).is_some(),
        "a WM close must not end the scratch shell"
    );
    assert!(
        !ctrl.quick_visible(),
        "a WM close must clear the summoned flag"
    );
    let (_, show) = ctrl.toggle_quick_terminal().expect("summon after WM close");
    assert!(
        show,
        "the next toggle after a WM close must summon, not dismiss"
    );
    settle_gtk(10);
    assert!(gtk_win.is_visible());

    ctrl.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}
