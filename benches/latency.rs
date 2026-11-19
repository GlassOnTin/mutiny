//! Headless latency and throughput baseline for the pane engine.
//!
//! Run with `cargo bench`. No GTK and no display are needed: this measures the
//! engine cost only. The render path is not measured here because it requires
//! a display; see benches/BASELINE.md for what is and is not covered.
//!
//! Numbers are recorded so a later change can be compared against a fixed
//! point rather than against a feeling.

use std::time::{Duration, Instant};

use mutiny::engine::{GridSize, KeyInput, KeyKind, PaneEngine, SEARCH_LINES_PER_TICK};

const COLS: usize = 120;
const ROWS: usize = 40;

fn main() {
    println!("mutiny engine baseline (headless, {COLS}x{ROWS} grid)");
    println!("{}", "-".repeat(72));

    ingest_throughput();
    input_to_grid_latency();
    pty_echo_latency();
    snapshot_cost();
    scrollback_search_cost();
    search_chunk_budget();
    render_damage_benefit();

    println!("{}", "-".repeat(72));
    println!("engine-only figures exclude GTK rendering, which needs a display.");
}

fn stats(mut samples: Vec<Duration>) -> (Duration, Duration, Duration) {
    samples.sort();
    let median = samples[samples.len() / 2];
    let p95 = samples[((samples.len() as f64 * 0.95) as usize).min(samples.len() - 1)];
    let min = samples[0];
    (median, p95, min)
}

fn fmt_us(d: Duration) -> String {
    format!("{:.1} us", d.as_secs_f64() * 1e6)
}

/// A realistic mixed stream: plain text, SGR colour runs, and OSC 133 markers
/// arriving chunked the way a PTY read delivers them.
fn mixed_stream(lines: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..lines {
        out.extend_from_slice(b"\x1b]133;A\x07$ ");
        out.extend_from_slice(format!("command --flag {i}\r\n").as_bytes());
        out.extend_from_slice(b"\x1b]133;C\x07");
        for row in 0..6 {
            out.extend_from_slice(b"\x1b[32mok\x1b[0m  ");
            out.extend_from_slice(
                format!("line {row} of output for command {i} with some trailing text\r\n")
                    .as_bytes(),
            );
        }
        out.extend_from_slice(b"\x1b]133;D;0\x07");
    }
    out
}

fn ingest_throughput() {
    let stream = mixed_stream(400);
    let total_bytes = stream.len();
    let mut samples = Vec::new();

    for _ in 0..12 {
        let mut engine = PaneEngine::new_headless(COLS, ROWS);
        let start = Instant::now();
        // Feed in 32 KiB chunks, matching drain_pty's read buffer.
        for chunk in stream.chunks(32 * 1024) {
            engine.advance_bytes(chunk);
        }
        samples.push(start.elapsed());
    }

    let (median, p95, _) = stats(samples);
    let mib = total_bytes as f64 / (1024.0 * 1024.0);
    println!(
        "VT ingest            {:>10} median  {:>10} p95   {:.1} MiB/s  ({} KiB stream)",
        fmt_us(median),
        fmt_us(p95),
        mib / median.as_secs_f64(),
        total_bytes / 1024
    );
}

/// Time from handing input bytes to the engine until the resulting glyph is
/// present in the grid. This is the Mutiny-controlled half of input-to-glyph
/// latency; the PTY, kernel, and shell add their own time on top.
fn input_to_grid_latency() {
    let mut samples = Vec::new();
    for _ in 0..200 {
        let mut engine = PaneEngine::new_headless(COLS, ROWS);
        engine.advance_bytes(b"$ ");
        let key = engine
            .encode_key(KeyInput {
                kind: KeyKind::Char('x'),
                shift: false,
                ctrl: false,
                alt: false,
                super_key: false,
            })
            .expect("encode x");

        let start = Instant::now();
        // Simulate the echo arriving back from the PTY.
        engine.advance_bytes(&key);
        let elapsed = start.elapsed();
        // Confirmation read is outside the timing: it is the bench's
        // assertion, not work the terminal does on the input path.
        assert!(
            engine.screen_text().contains('x'),
            "echoed glyph must reach the grid"
        );
        samples.push(elapsed);
    }
    let (median, p95, min) = stats(samples);
    println!(
        "input bytes to grid  {:>10} median  {:>10} p95   {:>10} min",
        fmt_us(median),
        fmt_us(p95),
        fmt_us(min)
    );
}

/// End-to-end echo through a real PTY: write a byte, poll until the line
/// discipline echoes it back into the grid. Includes kernel and PTY cost.
fn pty_echo_latency() {
    let mut samples = Vec::new();
    let mut failures = 0usize;
    for _ in 0..40 {
        let Ok(mut engine) = PaneEngine::spawn_pty(
            GridSize::new(COLS, ROWS, 9, 18),
            None,
            Some((String::from("/bin/sh"), Vec::new())),
            9000,
            10_000,
        ) else {
            println!("PTY echo             skipped (cannot spawn /bin/sh)");
            return;
        };

        let start = Instant::now();
        if engine.write_to_pty(b"x").is_err() {
            failures += 1;
            continue;
        }
        let mut found = false;
        while start.elapsed() < Duration::from_secs(2) {
            let _ = engine.drain_pty();
            if engine.screen_text().contains('x') {
                found = true;
                break;
            }
        }
        if found {
            samples.push(start.elapsed());
        } else {
            failures += 1;
        }
    }

    if samples.is_empty() {
        println!("PTY echo             no samples ({failures} failures)");
        return;
    }
    let (median, p95, min) = stats(samples);
    println!(
        "PTY echo round trip  {:>10} median  {:>10} p95   {:>10} min  ({failures} failed)",
        fmt_us(median),
        fmt_us(p95),
        fmt_us(min)
    );
}

/// What one agent snapshot poll costs on a populated grid.
fn snapshot_cost() {
    let mut engine = PaneEngine::new_headless(COLS, ROWS);
    engine.advance_bytes(&mixed_stream(200));
    let mut samples = Vec::new();
    for _ in 0..200 {
        let start = Instant::now();
        let snap = engine.snapshot(None);
        samples.push(start.elapsed());
        assert!(!snap.lines.is_empty());
    }
    let (median, p95, _) = stats(samples);
    println!(
        "pane snapshot        {:>10} median  {:>10} p95",
        fmt_us(median),
        fmt_us(p95)
    );
}

/// Search across a full 10 000-line scrollback. Two cases matter: a needle
/// that hits near the live screen exits early, and a needle that is absent
/// scans every line. The miss is the worst case.
fn scrollback_search_cost() {
    let mut engine = PaneEngine::new_headless(COLS, ROWS);
    engine.advance_bytes(&mixed_stream(2000));
    let history = engine.snapshot(None).scrollback_lines;

    let mut hit_samples = Vec::new();
    for _ in 0..20 {
        let start = Instant::now();
        let hits = engine.search_scrollback("trailing text", true, 20);
        hit_samples.push(start.elapsed());
        assert!(!hits.is_empty());
    }
    let (hit_median, hit_p95, _) = stats(hit_samples);

    let mut miss_samples = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let hits = engine.search_scrollback("needle-that-is-not-present", true, 20);
        miss_samples.push(start.elapsed());
        assert!(hits.is_empty());
    }
    let (miss_median, miss_p95, _) = stats(miss_samples);

    println!(
        "search hit (early)   {:>10} median  {:>10} p95   ({history} lines history)",
        fmt_us(hit_median),
        fmt_us(hit_p95)
    );
    println!(
        "search miss (full)   {:>10} median  {:>10} p95   (worst case)",
        fmt_us(miss_median),
        fmt_us(miss_p95)
    );
}

/// Tick-budget evidence for the chunked scan. Times each individual chunk of a
/// full-history no-match search, which is the work one GUI tick performs. The
/// whole-scan figure above is what the chunking replaced.
fn search_chunk_budget() {
    let mut engine = PaneEngine::new_headless(COLS, ROWS);
    engine.advance_bytes(&mixed_stream(2000));
    let history = engine.snapshot(None).scrollback_lines;

    engine.begin_search("needle-that-is-not-present", true, 20);
    let mut chunk_times = Vec::new();
    let total_start = Instant::now();
    let mut chunks = 0usize;
    loop {
        let start = Instant::now();
        let progress = engine
            .advance_search(SEARCH_LINES_PER_TICK)
            .expect("active search");
        chunk_times.push(start.elapsed());
        chunks += 1;
        if progress.done {
            break;
        }
        assert!(chunks < 10_000, "scan must terminate");
    }
    let total = total_start.elapsed();
    let (median, p95, _) = stats(chunk_times.clone());
    let max = *chunk_times.iter().max().expect("at least one chunk");
    let tick = Duration::from_micros(4_000);

    println!(
        "search chunk budget  {:>10} median  {:>10} p95   {:>10} max  ({} lines/chunk, {} history)",
        fmt_us(median),
        fmt_us(p95),
        fmt_us(max),
        SEARCH_LINES_PER_TICK,
        history
    );
    println!(
        "  full miss spread over {chunks} ticks ({} total); max chunk {:.1}% of the 4 ms tick: {}",
        fmt_us(total),
        max.as_secs_f64() / tick.as_secs_f64() * 100.0,
        if max < tick {
            "WITHIN BUDGET"
        } else {
            "OVER BUDGET"
        }
    );
    assert!(engine.search().expect("search").matches().is_empty());
}

/// Render-path cost with and without damage. Needs a display, so it skips in a
/// bare headless run; measure it under Xvfb. This is the cost centre the
/// damage-tracking rung addressed: rebuilding every row's Pango layouts on
/// every frame versus reusing cached row nodes.
fn render_damage_benefit() {
    use gtk4::prelude::*;
    if gtk4::init().is_err() {
        println!("render damage        skipped (no display; run under Xvfb)");
        return;
    }
    let dir = std::env::temp_dir().join(format!("mutiny-bench-render-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ctrl = mutiny::app::AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        mutiny::config::Overrides::default(),
    );
    let (_w, _t, pid) = ctrl.create_window(None).expect("pane");
    let view = mutiny::ui::terminal_view::TerminalView::new(pid, &ctrl);
    view.size_allocate(&gtk4::gdk::Rectangle::new(0, 0, 1200, 800), -1);
    while gtk4::glib::MainContext::default().iteration(false) {}

    // Fill the screen so rows have real text runs to shape.
    let mut buf = String::new();
    for i in 0..200 {
        buf.push_str(&format!(
            "\x1b[32mline {i}\x1b[0m some output text here\r\n"
        ));
    }
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .advance_bytes(buf.as_bytes());

    let render = || {
        let snap = gtk4::Snapshot::new();
        view.render_snapshot(&snap);
        let _ = snap.to_node();
        view.rows_rebuilt_last_frame()
    };
    render(); // warm the cache

    let rows = ctrl.workspace.borrow().pane(pid).unwrap().size().rows;
    let mut idle = Vec::new();
    let mut idle_rows = 0usize;
    for _ in 0..50 {
        let t = Instant::now();
        idle_rows = render();
        idle.push(t.elapsed());
    }
    let (idle_med, idle_p95, _) = stats(idle);

    let mut changed = Vec::new();
    let mut changed_rows = 0usize;
    for i in 0..50 {
        ctrl.workspace
            .borrow_mut()
            .pane_mut(pid)
            .unwrap()
            .advance_bytes(format!("\x1b[10;1Hupd {i}\r\n").as_bytes());
        let t = Instant::now();
        changed_rows = render();
        changed.push(t.elapsed());
    }
    let (chg_med, chg_p95, _) = stats(changed);

    println!(
        "render idle          {:>10} median  {:>10} p95   ({idle_rows}/{rows} rows rebuilt)",
        fmt_us(idle_med),
        fmt_us(idle_p95)
    );
    println!(
        "render changed       {:>10} median  {:>10} p95   ({changed_rows}/{rows} rows rebuilt)",
        fmt_us(chg_med),
        fmt_us(chg_p95)
    );
    // What the old full-frame repaint cost: invalidate the cache every frame.
    let mut full = Vec::new();
    for _ in 0..30 {
        view.invalidate_render_cache();
        let t = Instant::now();
        render();
        full.push(t.elapsed());
    }
    let (full_med, full_p95, _) = stats(full);
    println!(
        "render full repaint  {:>10} median  {:>10} p95   ({rows}/{rows} rows, the old path)",
        fmt_us(full_med),
        fmt_us(full_p95)
    );
    println!(
        "  idle {:.1}x cheaper than full repaint; changed {:.1}x",
        full_med.as_secs_f64() / idle_med.as_secs_f64().max(1e-9),
        full_med.as_secs_f64() / chg_med.as_secs_f64().max(1e-9)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
