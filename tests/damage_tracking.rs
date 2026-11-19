//! Damage-driven redraw: a no-change frame must rebuild no rows, and a changed
//! frame must rebuild only the damaged rows. Needs GTK, so it skips without a
//! display; CI runs it under Xvfb. One test function because GTK needs a single
//! thread and libtest spawns one per test.

use gtk4::prelude::*;
use mutiny::app::AppController;
use mutiny::config::Overrides;
use mutiny::topology::PaneId;
use mutiny::ui::terminal_view::TerminalView;

fn render(view: &TerminalView) -> usize {
    let snap = gtk4::Snapshot::new();
    view.render_snapshot(&snap);
    let _ = snap.to_node();
    view.rows_rebuilt_last_frame()
}

#[test]
fn no_change_frame_skips_rebuild_and_changed_frame_rebuilds_only_damaged_rows() {
    if gtk4::init().is_err() {
        eprintln!("skipping: no display (CI runs under Xvfb)");
        return;
    }
    let dir = std::env::temp_dir().join(format!("mutiny-damage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        Overrides::default(),
    );
    let (_w, _t, pid): (mutiny::topology::WindowId, mutiny::topology::TabId, PaneId) =
        ctrl.create_window(None).expect("pane");
    let view = TerminalView::new(pid, &ctrl);
    view.size_allocate(&gtk4::gdk::Rectangle::new(0, 0, 900, 600), -1);
    while gtk4::glib::MainContext::default().iteration(false) {}

    let rows = ctrl.workspace.borrow().pane(pid).unwrap().size().rows;

    // First frame builds everything.
    let first = render(&view);
    assert_eq!(first, rows, "first frame must build every row, got {first}");

    // No change: alacritty re-marks the cursor line on every damage() call, so
    // an idle frame rebuilds at most that one row, never the whole frame. The
    // cursor itself is drawn as an overlay, so this is the residual cost.
    let idle = render(&view);
    assert!(
        idle <= 1,
        "a no-change frame must rebuild at most the cursor row, got {idle} of {rows}"
    );
    let idle2 = render(&view);
    assert!(
        idle2 <= 1,
        "repeated idle frames must stay cheap, got {idle2}"
    );
    eprintln!("idle frame rebuilt {idle} of {rows} rows");

    // Change exactly one row: only that row rebuilds.
    {
        let mut ws = ctrl.workspace.borrow_mut();
        let eng = ws.pane_mut(pid).unwrap();
        // Move the cursor to row 3 and write there, damaging that line only.
        eng.advance_bytes(b"\x1b[4;1H");
        eng.advance_bytes(b"X");
    }
    let changed = render(&view);
    assert!(
        changed > 0 && changed < rows,
        "a single-line change must rebuild only part of the frame, got {changed} of {rows}"
    );
    eprintln!("single-line change rebuilt {changed} of {rows} rows");

    // After that, idle again.
    assert!(render(&view) <= 1, "must return to cheap idle frames");

    // Scrolling invalidates everything, because every row shows different content.
    {
        let mut ws = ctrl.workspace.borrow_mut();
        let eng = ws.pane_mut(pid).unwrap();
        let mut buf = String::new();
        for i in 0..(rows * 3) {
            buf.push_str(&format!("scroll line {i}\r\n"));
        }
        eng.advance_bytes(buf.as_bytes());
        eng.scroll(mutiny::engine::GridScroll::Delta(5));
    }
    let scrolled = render(&view);
    assert_eq!(
        scrolled, rows,
        "a scroll must rebuild every row, got {scrolled}"
    );

    // A config change (font) invalidates everything too.
    let cfg_path = dir.join("config.toml");
    std::fs::write(&cfg_path, "[font]\nsize = 16\n").unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
    std::fs::File::options()
        .write(true)
        .open(&cfg_path)
        .unwrap()
        .set_modified(later)
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(320));
    assert!(ctrl.reload_config_if_changed(), "config must reload");
    view.refresh_font();
    // A larger font changes cell metrics, so the grid has fewer rows now.
    // Compare against the live count, not the original one.
    let rows_now = ctrl.workspace.borrow().pane(pid).unwrap().size().rows;
    let restyled = render(&view);
    assert_eq!(
        restyled, rows_now,
        "a font change must rebuild every current row, got {restyled} of {rows_now}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
