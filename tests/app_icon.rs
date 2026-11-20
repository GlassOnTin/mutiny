//! The app icon must actually resolve through GTK's icon theme, not just exist
//! on disk. Needs a display; CI runs it under Xvfb. One test function because
//! GTK needs a single thread.

use gtk4::prelude::*;

#[test]
fn gtk_resolves_the_app_icon_from_the_embedded_assets() {
    if gtk4::init().is_err() {
        eprintln!("skipping: no display (CI runs under Xvfb)");
        return;
    }
    let display = gtk4::gdk::Display::default().expect("display");
    let theme = gtk4::IconTheme::for_display(&display);
    let path = mutiny::desktop::ensure_icon_search_path().expect("stage icons");
    theme.add_search_path(&path);

    for size in [16, 32, 128] {
        let paintable = theme.lookup_icon(
            mutiny::desktop::APP_ID,
            &[],
            size,
            1,
            gtk4::TextDirection::None,
            gtk4::IconLookupFlags::empty(),
        );
        let resolved = paintable.file().and_then(|f| f.path()).unwrap_or_else(|| {
            panic!(
                "GTK could not resolve {} at {size}px via {}",
                mutiny::desktop::APP_ID,
                path.display()
            )
        });
        assert!(
            resolved.to_string_lossy().contains(mutiny::desktop::APP_ID),
            "resolved the wrong icon: {}",
            resolved.display()
        );
        eprintln!("{size}px -> {}", resolved.display());
    }

    // A real Mutiny window must advertise the icon name to the shell. The
    // global default alone is not observable per window, so it is set on each
    // window too.
    gtk4::Window::set_default_icon_name(mutiny::desktop::APP_ID);
    let dir = std::env::temp_dir().join(format!("mutiny-appicon-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let app = gtk4::Application::new(
        Some(mutiny::desktop::APP_ID),
        gtk4::gio::ApplicationFlags::NON_UNIQUE,
    );
    app.register(gtk4::gio::Cancellable::NONE)
        .expect("register");
    let ctrl = mutiny::app::AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        mutiny::config::Overrides::default(),
    );
    ctrl.attach_gtk_app(&app);
    let (w, _t, _p) = ctrl.create_window(None).expect("window");
    while gtk4::glib::MainContext::default().iteration(false) {}
    let window = ctrl.ui_window(w).expect("view");
    assert_eq!(
        window.icon_name().as_deref(),
        Some(mutiny::desktop::APP_ID),
        "the window must advertise the app icon name"
    );
    ctrl.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}
