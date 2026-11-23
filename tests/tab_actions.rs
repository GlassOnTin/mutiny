//! The tab context-menu actions must actually fire. A menu item that renders
//! but does nothing is worse than no menu, and a screenshot cannot tell the
//! difference, so this drives the real gio actions rather than the mouse.
//!
//! Needs GTK; skips without a display. One test function because GTK needs a
//! single thread.

use std::rc::Rc;

use gtk4::prelude::*;
use mutiny::app::AppController;
use mutiny::config::Overrides;

fn settle(ctrl: &Rc<AppController>, rounds: usize) {
    for _ in 0..rounds {
        ctrl.poll_once();
        while gtk4::glib::MainContext::default().iteration(false) {}
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn tab_targeted_actions_fire_and_a_rename_dialog_opens() {
    if gtk4::init().is_err() {
        eprintln!("skipping: no display (CI runs under Xvfb)");
        return;
    }
    let dir = std::env::temp_dir().join(format!("mutiny-tabact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // A fake tmux so the pane can be "attached" without a real server.
    let fake = dir.join("fake-tmux");
    // When exec'd as an attach it must stay alive, or the pane exits and the
    // tab vanishes before the action can be driven.
    std::fs::write(
        &fake,
        r#"#!/bin/sh
printf 'alpha\t1\t0\t1\t/x\n'
case "$1" in
  new-session) exec sleep 60 ;;
esac
"#,
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        dir.join("config.toml"),
        format!(
            concat!(
                "[sessions]\n",
                "tmux_program = {:?}\n",
                "zellij_program = \"/nonexistent-zellij\"\n",
                "ssh_program = \"/nonexistent-ssh\"\n",
                "preferred = \"none\"\n",
                "refresh_interval_s = 0\n",
            ),
            fake.display().to_string()
        ),
    )
    .unwrap();

    let app = gtk4::Application::new(
        Some("dev.mutiny.tabact.test"),
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
    let (w, t_plain, _p) = ctrl.create_window(None).expect("window");
    let (_w, t_attached, p_attached) = ctrl
        .open_session(
            mutiny::mux::MuxKind::Tmux,
            "alpha",
            None,
            mutiny::mux::SessionMode::Tab,
            Some(w),
            None,
        )
        .expect("attach");
    settle(&ctrl, 12);
    let window = ctrl.ui_window(w).expect("view");
    let gtk_win = window.gtk_window();

    // The menu must be parented to the WINDOW and stay parented after it
    // closes. Parenting it to a tab button and unparenting on `closed` broke
    // every item, because GTK emits `closed` before activating the action.
    let menu = window.tab_menu();
    assert_eq!(
        menu.parent(),
        Some(gtk_win.clone().upcast::<gtk4::Widget>()),
        "the tab menu must be parented to the window, not a tab button"
    );
    menu.popup();
    settle(&ctrl, 6);
    menu.popdown();
    settle(&ctrl, 6);
    assert!(
        menu.parent().is_some(),
        "the menu must stay parented after closing, or its actions cannot resolve"
    );

    // Rename on an attached tab must open a dialog window.
    let before = gtk4::Window::toplevels().n_items();
    gtk4::prelude::WidgetExt::activate_action(
        &gtk_win,
        "win.rename-session-tab",
        Some(&gtk4::glib::Variant::from(t_attached.0)),
    )
    .expect("the action must exist and activate");
    settle(&ctrl, 12);
    let after = gtk4::Window::toplevels().n_items();
    assert!(
        after > before,
        "the rename action must open a dialog: toplevels {before} -> {after}"
    );
    // Close it so the next assertion is clean.
    let mut closed = false;
    for i in 0..gtk4::Window::toplevels().n_items() {
        if let Some(w) = gtk4::Window::toplevels()
            .item(i)
            .and_downcast::<gtk4::Window>()
            && w.title().as_deref() == Some("Rename Session")
        {
            w.close();
            closed = true;
        }
    }
    assert!(closed, "the dialog must be titled Rename Session");
    settle(&ctrl, 8);

    // Rename on a PLAIN tab must do nothing: there is no session to rename.
    let before = gtk4::Window::toplevels().n_items();
    gtk4::prelude::WidgetExt::activate_action(
        &gtk_win,
        "win.rename-session-tab",
        Some(&gtk4::glib::Variant::from(t_plain.0)),
    )
    .expect("the action must exist and activate");
    settle(&ctrl, 8);
    assert_eq!(
        gtk4::Window::toplevels().n_items(),
        before,
        "a plain tab has no session to rename"
    );

    // New Session opens its dialog too.
    let before = gtk4::Window::toplevels().n_items();
    gtk4::prelude::WidgetExt::activate_action(&gtk_win, "win.new-session", None)
        .expect("the action must exist and activate");
    settle(&ctrl, 10);
    assert!(
        gtk4::Window::toplevels().n_items() > before,
        "new-session must open a dialog"
    );
    for i in 0..gtk4::Window::toplevels().n_items() {
        if let Some(w) = gtk4::Window::toplevels()
            .item(i)
            .and_downcast::<gtk4::Window>()
            && w.title().as_deref() == Some("New Session")
        {
            w.close();
        }
    }
    settle(&ctrl, 8);

    // Close Tab really closes it.
    gtk4::prelude::WidgetExt::activate_action(
        &gtk_win,
        "win.close-tab-target",
        Some(&gtk4::glib::Variant::from(t_attached.0)),
    )
    .expect("the action must exist and activate");
    settle(&ctrl, 10);
    assert!(
        ctrl.workspace
            .borrow()
            .window(w)
            .map(|wm| wm.tabs.iter().all(|t| t.id != t_attached))
            .unwrap_or(true),
        "close-tab-target must remove the tab"
    );
    let _ = p_attached;

    ctrl.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}
