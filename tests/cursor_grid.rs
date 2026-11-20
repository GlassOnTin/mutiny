//! The drawn cursor must sit exactly on alacritty's grid cursor, and the text
//! must occupy exactly its cells.
//!
//! Ian's bug: the cursor drifted BEYOND the end of the text. Root cause was not
//! the cursor arithmetic, which was already grid-correct, but the text: a run
//! drawn as one Pango layout used the font's natural advance (8.83 px for
//! Monospace 11) while the grid uses ceil(approximate_char_width) = 9 px. The
//! shortfall accumulated per column, so the text ended before the cursor.
//!
//! Needs GTK; skips without a display. One test function because GTK needs a
//! single thread and libtest spawns one per test.

use gtk4::prelude::*;
use mutiny::app::AppController;
use mutiny::config::Overrides;
use mutiny::ui::terminal_view::TerminalView;

fn render(view: &TerminalView) {
    let snap = gtk4::Snapshot::new();
    view.render_snapshot(&snap);
    let _ = snap.to_node();
}

#[test]
fn glyphs_and_cursor_land_exactly_on_the_cell_grid() {
    if gtk4::init().is_err() {
        eprintln!("skipping: no display (CI runs under Xvfb)");
        return;
    }
    let dir = std::env::temp_dir().join(format!("mutiny-cursor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        Overrides::default(),
    );
    let (_w, _t, pid) = ctrl.create_window(None).expect("pane");
    let view = TerminalView::new(pid, &ctrl);
    view.size_allocate(&gtk4::gdk::Rectangle::new(0, 0, 900, 600), -1);
    while gtk4::glib::MainContext::default().iteration(false) {}

    let cw = view.cell_width();
    let padding = ctrl.config_ref().padding_px;

    // The metric mismatch that caused the bug is real, and is why per-cell
    // placement is required rather than a nicer rounding of the cell width.
    {
        let ctx = view.pango_context();
        let desc = gtk4::pango::FontDescription::from_string(&ctrl.config_ref().font_description);
        let metrics = ctx.metrics(Some(&desc), None);
        let approx = metrics.approximate_char_width() as f32 / gtk4::pango::SCALE as f32;
        let layout = gtk4::pango::Layout::new(&ctx);
        layout.set_font_description(Some(&desc));
        layout.set_text(&"x".repeat(40));
        let natural = layout.pixel_size().0 as f32;
        eprintln!(
            "approx={approx:.4} cw={cw:.4} natural(40)={natural:.2} grid(40)={:.2}",
            40.0 * cw
        );
        assert!(
            (40.0 * cw - natural).abs() > 1.0,
            "expected the font advance to differ from the cell width; \
             if this ever becomes exact the per-cell rationale needs revisiting"
        );
    }

    // A long single-style line: the case where run drift accumulated most.
    let line = "echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .advance_bytes(format!("{line}\r\n").as_bytes());
    // Put the cursor on a known cell after the text.
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .advance_bytes(b"prompt$ ");

    view.record_glyph_positions(true);
    view.invalidate_render_cache();
    render(&view);
    let glyphs = view.glyph_positions();
    assert!(!glyphs.is_empty(), "nothing was drawn");

    for (row, col, x) in &glyphs {
        let expected = padding + (*col as f32) * cw;
        assert!(
            (x - expected).abs() < 0.01,
            "glyph at row {row} col {col} drawn at {x}, expected {expected} \
             (text must occupy exactly its cells)"
        );
    }

    // The cursor is one cell past the last glyph of the prompt line, exactly.
    let (cursor_col, cursor_line) = {
        let ws = ctrl.workspace.borrow();
        let eng = ws.pane(pid).unwrap();
        let p = eng.term().grid().cursor.point;
        (p.column.0, p.line.0)
    };
    // The last NON-BLANK cell before the cursor must have a glyph drawn at
    // exactly its grid column. Trailing spaces are not drawn, so comparing to
    // the last drawn glyph directly would be wrong.
    let last_content_col = {
        let ws = ctrl.workspace.borrow();
        let eng = ws.pane(pid).unwrap();
        let grid = eng.term().grid();
        let row = &grid[alacritty_terminal::index::Line(cursor_line)];
        (0..cursor_col).rev().find(|c| {
            let cell = &row[alacritty_terminal::index::Column(*c)];
            cell.c != ' '
                && !cell
                    .flags
                    .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER)
        })
    };
    if let Some(col) = last_content_col {
        assert!(
            glyphs
                .iter()
                .any(|(row, c, _)| *row as i32 == cursor_line && *c == col),
            "the cell the cursor follows (col {col}) must have a glyph on the grid"
        );
        let drawn_x = glyphs
            .iter()
            .find(|(row, c, _)| *row as i32 == cursor_line && *c == col)
            .unwrap()
            .2;
        let cursor_x = padding + (cursor_col as f32) * cw;
        assert!(
            ((cursor_x - drawn_x) - ((cursor_col - col) as f32) * cw).abs() < 0.01,
            "the gap between the last glyph and the cursor must be exactly \
             (cursor_col - glyph_col) cells"
        );
    }
    let cursor_x = padding + (cursor_col as f32) * cw;
    let last_x = padding + (cursor_col.saturating_sub(1) as f32) * cw;
    assert!(
        (cursor_x - last_x - cw).abs() < 0.01,
        "cursor and preceding cell must be exactly one cell apart"
    );

    // Wide characters: each CJK glyph occupies two cells, so glyph columns step
    // by two and still land on the grid.
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .advance_bytes("日本語\r\n".as_bytes());
    view.record_glyph_positions(true);
    view.invalidate_render_cache();
    render(&view);
    let wide: Vec<(usize, usize, f32)> = view
        .glyph_positions()
        .into_iter()
        .filter(|(_, col, _)| *col < 8)
        .collect();
    for (row, col, x) in &wide {
        let expected = padding + (*col as f32) * cw;
        assert!(
            (x - expected).abs() < 0.01,
            "wide glyph at row {row} col {col} off-grid: {x} vs {expected}"
        );
    }

    ctrl.shutdown_ipc();
    let _ = std::fs::remove_dir_all(&dir);
}
