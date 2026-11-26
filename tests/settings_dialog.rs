//! The Settings dialog must expose every config key and write a file the
//! loader accepts. Needs GTK; skips without a display. One test function
//! because GTK needs a single thread.

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

/// Counts editable widgets in a widget tree, so the coverage guard does not
/// depend on layout details.
fn count_inputs(w: &gtk4::Widget) -> usize {
    let mut n = 0;
    if w.is::<gtk4::Entry>()
        || w.is::<gtk4::SpinButton>()
        || w.is::<gtk4::CheckButton>()
        || w.is::<gtk4::DropDown>()
    {
        n += 1;
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        n += count_inputs(&c);
        child = c.next_sibling();
    }
    n
}

#[test]
fn settings_dialog_covers_every_key_and_saves_a_loadable_file() {
    if gtk4::init().is_err() {
        eprintln!("skipping: no display (CI runs under Xvfb)");
        return;
    }
    let dir = std::env::temp_dir().join(format!("mutiny-settings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_path = dir.join("config.toml");

    let app = gtk4::Application::new(
        Some("dev.mutiny.settings.test"),
        gtk4::gio::ApplicationFlags::NON_UNIQUE,
    );
    app.register(gtk4::gio::Cancellable::NONE)
        .expect("register");
    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        cfg_path.clone(),
        Overrides::default(),
    );
    ctrl.attach_gtk_app(&app);
    let (w, _t, _p) = ctrl.create_window(None).expect("window");
    settle(&ctrl, 10);

    let before = gtk4::Window::toplevels().n_items();
    ctrl.execute_action(mutiny::action::ActionId::Settings, Some(w));
    settle(&ctrl, 12);
    assert!(
        gtk4::Window::toplevels().n_items() > before,
        "Settings must open a dialog"
    );

    let dialog = (0..gtk4::Window::toplevels().n_items())
        .filter_map(|i| {
            gtk4::Window::toplevels()
                .item(i)
                .and_downcast::<gtk4::Window>()
        })
        .find(|w| w.title().as_deref() == Some("Settings"))
        .expect("a dialog titled Settings");

    // font 2 + colors 5 + palette 16 + geometry 6 + terminal 1 + sessions 13 + peer 2 + notifications 3
    let inputs = count_inputs(dialog.upcast_ref::<gtk4::Widget>());
    assert!(
        inputs >= 48,
        "the dialog must expose every config key, found only {inputs} inputs"
    );

    // Save with no edits must write a file the loader accepts, unchanged.
    let mut save = None;
    let mut stack = vec![dialog.clone().upcast::<gtk4::Widget>()];
    while let Some(w) = stack.pop() {
        if let Some(b) = w.downcast_ref::<gtk4::Button>()
            && b.label().as_deref() == Some("Save")
        {
            save = Some(b.clone());
        }
        let mut c = w.first_child();
        while let Some(child) = c {
            stack.push(child.clone());
            c = child.next_sibling();
        }
    }
    save.expect("a Save button").emit_clicked();
    settle(&ctrl, 10);

    assert!(cfg_path.is_file(), "Save must write the config file");
    let written = mutiny::config::load_or_default(&cfg_path);
    assert_eq!(
        written,
        mutiny::config::Config::default(),
        "saving an untouched dialog must not change any value"
    );

    ctrl.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}
