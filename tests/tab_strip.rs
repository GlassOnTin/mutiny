//! The tab strip must adapt to the window, never stretch it. Ian's dogfooding
//! bug: opening ~17 session tabs made the window grow horizontally, because a
//! horizontal Box's minimum width is the sum of its children.
//!
//! Needs GTK, so it skips without a display; CI runs it under Xvfb. One test
//! function because GTK needs a single thread and libtest spawns one per test.

use std::rc::Rc;

use gtk4::prelude::*;
use mutiny::app::AppController;
use mutiny::config::Overrides;

fn settle(ctrl: &Rc<AppController>, rounds: usize) {
    for _ in 0..rounds {
        ctrl.poll_once();
        while gtk4::glib::MainContext::default().iteration(false) {}
    }
}

#[test]
fn many_tabs_wrap_without_stretching_the_window() {
    if gtk4::init().is_err() {
        eprintln!("skipping: no display (CI runs under Xvfb)");
        return;
    }
    let dir = std::env::temp_dir().join(format!("mutiny-tabstrip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let app = gtk4::Application::new(
        Some("dev.mutiny.tabstrip.test"),
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
    settle(&ctrl, 20);
    let window = ctrl.ui_window(w).expect("view");
    window.rebuild_from_model();
    settle(&ctrl, 20);

    let baseline_min = window.window_minimum_width();
    let baseline_rows = window.tab_strip_row_count();
    assert_eq!(baseline_rows, 1, "one tab is one row");

    // Ian's case: about seventeen session tabs.
    for _ in 0..16 {
        ctrl.create_tab(Some(w), None).expect("tab");
    }
    settle(&ctrl, 40);

    let after_min = window.window_minimum_width();
    let rows = window.tab_strip_row_count();
    let strip_width = window.tab_strip_width();
    eprintln!(
        "min width: {baseline_min} -> {after_min}; rows: {baseline_rows} -> {rows}; strip width {strip_width}; min height {}; strip height {}",
        window.window_minimum_height(),
        window.tab_strip_height()
    );

    // The strip's minimum contribution is ONE tab, not the sum of all of them.
    // A small increase is legitimate: label "17: shell" is wider than "1: shell".
    let one_tab = window.widest_tab_minimum_width();
    assert!(
        after_min <= baseline_min + one_tab,
        "minimum width must grow by at most one tab, not by the tab count: \
         {baseline_min} -> {after_min} (one tab is {one_tab})"
    );
    assert!(
        rows > 1,
        "seventeen tabs must wrap to more than one row, got {rows}"
    );

    // The decisive property: the minimum PLATEAUS. Adding still more tabs must
    // not widen the window further, which a sum-of-children Box cannot do.
    for _ in 0..20 {
        ctrl.create_tab(Some(w), None).expect("tab");
    }
    settle(&ctrl, 40);
    let after_more = window.window_minimum_width();
    eprintln!(
        "37 tabs: min height {} strip height {} rows {}",
        window.window_minimum_height(),
        window.tab_strip_height(),
        window.tab_strip_row_count()
    );
    // Bounded, not exactly equal: the strip's minimum is its widest SINGLE tab,
    // and which label is widest can shift by a few pixels as titles change.
    // Twenty more tabs must not add twenty tabs' worth of width.
    assert!(
        (after_more - after_min).abs() <= one_tab,
        "twenty more tabs must not widen the window by more than one tab: \
         {after_min} -> {after_more} (one tab is {one_tab})"
    );
    assert!(
        after_more <= baseline_min + one_tab,
        "the bound must hold at 37 tabs too: {baseline_min} -> {after_more}"
    );
    assert!(
        window.tab_strip_row_count() > rows,
        "more tabs must add rows, not width"
    );

    // Wrapping trades width for height, so the strip must be capped and scroll
    // rather than squeezing the terminal out of the window.
    let cap = ctrl.config_ref().tab_strip_max_height_px;
    let visible = window.tab_strip_height();
    let content = window.tab_strip_content_height();
    assert!(
        visible <= cap + 2,
        "the visible strip must respect the height cap {cap}, got {visible}"
    );
    assert!(
        content > visible,
        "with 37 tabs the content should overflow and scroll: content {content}, visible {visible}"
    );

    // Usability must survive the widget change: clicking a wrapped tab still
    // focuses it, and the active tab is still marked.
    let target = ctrl
        .workspace
        .borrow()
        .window(w)
        .unwrap()
        .tabs
        .last()
        .unwrap()
        .id;
    let btn = window.tab_button(target).expect("tab button exists");
    btn.emit_clicked();
    settle(&ctrl, 10);
    assert_eq!(
        ctrl.workspace.borrow().window(w).unwrap().active_tab,
        target,
        "clicking a wrapped tab must focus it"
    );
    let marked = window
        .tab_button(target)
        .expect("button")
        .has_css_class("suggested-action");
    assert!(
        marked,
        "the active tab must still be marked after a rebuild"
    );
    let labels = window.tab_labels();
    assert!(
        labels.iter().any(|l| l.starts_with("37:")),
        "every tab must still be labelled and numbered: {labels:?}"
    );

    // Removing tabs must not leave the window stretched either.
    // Closing a tab's last pane removes the tab, which is the only close path.
    let panes: Vec<mutiny::topology::PaneId> = {
        let ws = ctrl.workspace.borrow();
        let win = ws.window(w).unwrap();
        win.tabs
            .iter()
            .skip(1)
            .flat_map(|t| t.root.leaves())
            .collect()
    };
    for pid in panes {
        ctrl.close_pane(pid);
    }
    settle(&ctrl, 30);
    assert!(
        window.window_minimum_width() <= baseline_min + one_tab,
        "width must stay stable as tabs are removed"
    );

    ctrl.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}
