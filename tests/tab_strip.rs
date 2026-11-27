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

    // Toolbar compaction: glyph-only buttons, measured against the same
    // buttons with the words they used to carry.
    let compact = window.toolbar_width();
    let labelled = {
        let probe = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
        for text in ["+ Tab", "Split →", "Split ↓", "Detach ↗", "⌘ Palette", "?"] {
            probe.append(&gtk4::Button::with_label(text));
        }
        let win = gtk4::Window::new();
        win.set_child(Some(&probe));
        win.present();
        while gtk4::glib::MainContext::default().iteration(false) {}
        let w = probe.measure(gtk4::Orientation::Horizontal, -1).0;
        win.close();
        w
    };
    eprintln!("toolbar: labelled {labelled}px -> compact {compact}px");
    assert!(
        compact < labelled,
        "the compact toolbar must be narrower: {compact} vs {labelled}"
    );

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
    assert_eq!(labels.len(), 37, "every tab must still be labelled");
    assert!(
        !labels.iter().any(|l| l.starts_with("37:")),
        "session numbers are dropped as redundant noise: {labels:?}"
    );
    assert!(
        labels.iter().all(|l| !l.is_empty()),
        "no tab may be unlabelled: {labels:?}"
    );

    // Session tab labels and padding scale with the window's terminal font size:
    // zooming out shrinks individual tab width and total strip height (packing
    // more tabs per row), zooming in widens them, and reset restores baseline.
    let base_tab_w = window.widest_tab_minimum_width();
    let base_content_h = window.tab_strip_content_height();
    ctrl.zoom_window(w, -4);
    for _ in 0..8 {
        std::thread::sleep(std::time::Duration::from_millis(8));
        settle(&ctrl, 5);
    }
    let zoomed_out_tab_w = window.widest_tab_minimum_width();
    let zoomed_out_content_h = window.tab_strip_content_height();
    assert!(
        zoomed_out_tab_w < base_tab_w,
        "zooming out must shrink session tab width: {base_tab_w} -> {zoomed_out_tab_w}"
    );
    assert!(
        zoomed_out_content_h < base_content_h,
        "zooming out must reduce wrapped tab strip content height: {base_content_h} -> {zoomed_out_content_h}"
    );

    ctrl.zoom_window(w, 12);
    for _ in 0..8 {
        std::thread::sleep(std::time::Duration::from_millis(8));
        settle(&ctrl, 5);
    }
    let zoomed_in_tab_w = window.widest_tab_minimum_width();
    assert!(
        zoomed_in_tab_w > base_tab_w,
        "zooming in must grow session tab width: {base_tab_w} -> {zoomed_in_tab_w}"
    );

    ctrl.reset_window_zoom(w);
    for _ in 0..8 {
        std::thread::sleep(std::time::Duration::from_millis(8));
        settle(&ctrl, 5);
    }
    assert_eq!(
        window.widest_tab_minimum_width(),
        base_tab_w,
        "resetting zoom must restore baseline session tab width"
    );

    // View -> Toggle Tab Hostnames hides the second-line hostname/shell subtitle
    // and drops each tab button's vertical minimum height significantly.
    let two_line_btn_h = window
        .tab_button(target)
        .unwrap()
        .measure(gtk4::Orientation::Vertical, -1)
        .0;
    ctrl.execute_action(mutiny::action::ActionId::ToggleTabHostnames, Some(w));
    for _ in 0..8 {
        std::thread::sleep(std::time::Duration::from_millis(8));
        settle(&ctrl, 5);
    }
    let one_line_btn_h = window
        .tab_button(target)
        .unwrap()
        .measure(gtk4::Orientation::Vertical, -1)
        .0;
    assert!(
        one_line_btn_h < (two_line_btn_h * 3) / 4,
        "hiding tab hostnames must drop tab height substantially: {two_line_btn_h} -> {one_line_btn_h}"
    );
    ctrl.execute_action(mutiny::action::ActionId::ToggleTabHostnames, Some(w));
    for _ in 0..8 {
        std::thread::sleep(std::time::Duration::from_millis(8));
        settle(&ctrl, 5);
    }
    assert_eq!(
        window
            .tab_button(target)
            .unwrap()
            .measure(gtk4::Orientation::Vertical, -1)
            .0,
        two_line_btn_h,
        "re-enabling tab hostnames must restore two-line tab height"
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
