//! IME commit path and font fallback.
//!
//! GTK must be initialized and used from one thread, and libtest runs each
//! test on its own thread, so every GTK assertion lives in a single test
//! function. Skips without a display; CI runs the suite under Xvfb.

use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use mutiny::app::AppController;
use mutiny::config::Overrides;
use mutiny::topology::PaneId;
use mutiny::ui::terminal_view::TerminalView;

fn pump(ctrl: &Rc<AppController>, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(10));
    }
}

fn drain_main_context() {
    while gtk4::glib::MainContext::default().iteration(false) {}
}

#[test]
fn ime_commit_path_and_font_fallback() {
    if gtk4::init().is_err() {
        eprintln!("skipping: no display (CI runs under Xvfb)");
        return;
    }

    // ---- Font fallback -------------------------------------------------
    // Pango resolves fallback through fontconfig. Zero unknown glyphs for a
    // script the base Monospace font does not cover means fallback engaged.
    // A bare pango::Context has no font map, which makes itemization a no-op
    // and unknown_glyphs_count vacuously zero. Use a real widget's context.
    let probe_widget = gtk4::Label::new(None);
    let ctx = probe_widget.pango_context();
    let layout = gtk4::pango::Layout::new(&ctx);
    layout.set_font_description(Some(&gtk4::pango::FontDescription::from_string(
        "Monospace 11",
    )));
    for (label, sample) in [
        ("CJK", "你好世界日本語"),
        ("emoji", "🙂🚀✅"),
        ("box drawing", "─│┌┐└┘├┤┬┴┼"),
        ("accented", "éñüßåø"),
    ] {
        layout.set_text(sample);
        assert_eq!(
            layout.unknown_glyphs_count(),
            0,
            "{label} must fall back to a covering font"
        );
    }

    // Fallback must pick a genuinely different font for CJK, not silently
    // render placeholders from the Latin base.
    layout.set_text("hello 你好");
    let mut fonts = Vec::new();
    let mut iter = layout.iter();
    loop {
        if let Some(run) = iter.run() {
            fonts.push(run.item().analysis().font().describe().to_str().to_string());
        }
        if !iter.next_run() {
            break;
        }
    }
    eprintln!("resolved runs: {fonts:?}");
    assert!(fonts.len() >= 2, "expected Latin and CJK runs: {fonts:?}");
    assert_ne!(
        fonts[0], fonts[1],
        "CJK must resolve to a different font than the Latin base: {fonts:?}"
    );

    // ---- IME wiring and commit path ------------------------------------
    let dir = std::env::temp_dir().join(format!("mutiny-ime-{}", std::process::id()));
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
    // No window: the commit path writes to the PTY and does not need the
    // widget realized. Creating a top-level here would also poke the user's
    // real compositor when run outside Xvfb.
    let view = TerminalView::new(pid, &ctrl);
    drain_main_context();

    // Regression: an unrealized view has a 0x0 allocation. It must not shrink
    // the pane to the minimum grid, which wrapped every wide glyph and ate
    // characters. The pane keeps its configured spawn size until allocated.
    let size = ctrl.workspace.borrow().pane(pid).unwrap().size();
    assert_eq!(
        (size.cols, size.rows),
        (100, 30),
        "an unrealized view must not resize the pane"
    );

    // Let the shell finish setting up its tty before typing, exactly as a
    // human would. Writing during startup races the shell's termios setup.
    pump(&ctrl, 600);

    let im = view
        .im_context()
        .expect("view must own an IM context for dead keys, compose, and CJK");

    // Composed text must reach the PTY as exact UTF-8 and echo back. This is
    // the path that was missing entirely before: with no IM context there was
    // nothing to compose with and nothing to deliver the result.
    for text in ["你好世界", "éñü", "🙂"] {
        im.emit_by_name::<()>("commit", &[&text]);
        pump(&ctrl, 500);
        let screen = ctrl.workspace.borrow().pane(pid).unwrap().screen_text();
        assert!(
            screen.contains(text),
            "committed {text:?} must appear in the pane; screen was:\n{screen}"
        );
        assert!(
            view.preedit_text().is_empty(),
            "preedit must clear after commit"
        );
    }

    // Preedit is tracked and cleared.
    im.emit_by_name::<()>("preedit-changed", &[]);
    assert!(view.preedit_text().is_empty());
    im.emit_by_name::<()>("commit", &[&"é"]);
    assert!(view.preedit_text().is_empty());

    drain_main_context();
    let _ = std::fs::remove_dir_all(&dir);
}
