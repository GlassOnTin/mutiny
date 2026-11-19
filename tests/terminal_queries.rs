//! Terminal capability query responses, and proof that fish 4 starts without
//! blocking on them. fish 4 waits for answers before it becomes responsive, so
//! a silent terminal leaves it alive but stuck.

use std::env;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use mutiny::engine::{GridSize, PaneEngine};

fn replies_to(query: &[u8]) -> String {
    let mut eng = PaneEngine::new_headless(80, 24);
    eng.advance_bytes(query);
    String::from_utf8_lossy(eng.pending_pty_writes()).into_owned()
}

#[test]
fn alacritty_handled_queries_are_answered_exactly_once() {
    // These are alacritty's job. Answering them again in PaneEngine would send
    // two replies and confuse the querying program.
    for (name, query, expected) in [
        ("primary DA", &b"\x1b[c"[..], "\x1b[?6c"),
        ("primary DA 0", &b"\x1b[0c"[..], "\x1b[?6c"),
        ("DSR cursor position", &b"\x1b[6n"[..], "\x1b[1;1R"),
        ("DSR status", &b"\x1b[5n"[..], "\x1b[0n"),
        ("kitty keyboard query", &b"\x1b[?u"[..], "\x1b[?0u"),
    ] {
        let reply = replies_to(query);
        assert_eq!(reply, expected, "{name} reply changed");
        assert_eq!(
            reply.matches(expected).count(),
            1,
            "{name} must be answered once, got {reply:?}"
        );
    }
}

#[test]
fn xtversion_reports_the_terminal_name_and_version() {
    for query in [&b"\x1b[>0q"[..], &b"\x1b[>q"[..]] {
        let reply = replies_to(query);
        assert!(
            reply.starts_with("\x1bP>|mutiny "),
            "XTVERSION reply must be DCS >| mutiny ...: {reply:?}"
        );
        assert!(
            reply.ends_with("\x1b\\"),
            "must terminate with ST: {reply:?}"
        );
        assert!(
            reply.contains(env!("CARGO_PKG_VERSION")),
            "must carry the crate version: {reply:?}"
        );
    }
}

#[test]
fn osc_color_queries_report_the_configured_colors() {
    let mut eng = PaneEngine::new_headless(80, 24);
    eng.set_reported_colors(
        mutiny::config::parse_hex("#102030").unwrap(),
        mutiny::config::parse_hex("#abcdef").unwrap(),
    );
    eng.advance_bytes(b"\x1b]11;?\x1b\\");
    let bg = String::from_utf8_lossy(eng.pending_pty_writes()).into_owned();
    assert_eq!(bg, "\x1b]11;rgb:1010/2020/3030\x1b\\");

    let mut eng = PaneEngine::new_headless(80, 24);
    eng.set_reported_colors(
        mutiny::config::parse_hex("#102030").unwrap(),
        mutiny::config::parse_hex("#abcdef").unwrap(),
    );
    eng.advance_bytes(b"\x1b]10;?\x07");
    let fg = String::from_utf8_lossy(eng.pending_pty_writes()).into_owned();
    assert_eq!(fg, "\x1b]10;rgb:abab/cdcd/efef\x1b\\");
}

#[test]
fn dcs_terminfo_answers_what_it_can_and_declines_the_rest() {
    // "indn" hex-encoded: we do not claim it, so decline. A decline is a valid
    // answer and unblocks the caller; silence is what blocks it.
    let declined = replies_to(b"\x1bP+q696e646e\x1b\\");
    assert_eq!(declined, "\x1bP0+r\x1b\\");

    // "query-os-name" hex-encoded: mutiny is Linux-only, so this is a fact.
    let answered = replies_to(b"\x1bP+q71756572792d6f732d6e616d65\x1b\\");
    assert!(answered.starts_with("\x1bP1+r"), "got {answered:?}");
    assert!(
        answered.contains("71756572792d6f732d6e616d65=6c696e7578"),
        "expected hex(query-os-name)=hex(linux): {answered:?}"
    );

    // Mixed query: one supported, one not.
    let mixed = replies_to(b"\x1bP+q696e646e;71756572792d6f732d6e616d65\x1b\\");
    assert!(mixed.starts_with("\x1bP1+r"), "got {mixed:?}");
    assert!(
        !mixed.contains("696e646e="),
        "must not claim indn: {mixed:?}"
    );
}

#[test]
fn queries_split_across_reads_are_still_answered() {
    let mut eng = PaneEngine::new_headless(80, 24);
    eng.advance_bytes(b"\x1b[>0");
    assert!(eng.pending_pty_writes().is_empty(), "no reply mid-sequence");
    eng.advance_bytes(b"q");
    let reply = String::from_utf8_lossy(eng.pending_pty_writes()).into_owned();
    assert!(reply.starts_with("\x1bP>|mutiny "), "got {reply:?}");

    let mut eng = PaneEngine::new_headless(80, 24);
    eng.advance_bytes(b"\x1bP+q696e");
    eng.advance_bytes(b"646e\x1b\\");
    let reply = String::from_utf8_lossy(eng.pending_pty_writes()).into_owned();
    assert_eq!(reply, "\x1bP0+r\x1b\\");
}

#[test]
fn ordinary_output_produces_no_query_replies() {
    let mut eng = PaneEngine::new_headless(80, 24);
    eng.advance_bytes(b"hello world\r\n\x1b[32mgreen\x1b[0m\r\n$ ");
    assert!(
        eng.pending_pty_writes().is_empty(),
        "plain output must not trigger replies: {:?}",
        String::from_utf8_lossy(eng.pending_pty_writes())
    );
}

fn find_fish() -> Option<PathBuf> {
    if let Ok(explicit) = env::var("MUTINY_TEST_FISH")
        && PathBuf::from(&explicit).is_file()
    {
        return Some(PathBuf::from(explicit));
    }
    if let Ok(out) = std::process::Command::new("sh")
        .args(["-c", "command -v fish"])
        .output()
        && out.status.success()
    {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    None
}

/// The done-when for this rung: a real fish 4 must start and process input
/// rather than blocking on unanswered capability queries. Skips when fish is
/// absent; CI installs it so the assertion actually runs there.
#[test]
fn fish4_starts_without_blocking_on_capability_queries() {
    let Some(fish) = find_fish() else {
        eprintln!("skipping: fish not installed (CI installs it)");
        return;
    };
    let version = std::process::Command::new(&fish)
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    eprintln!("testing against {version}");

    let mut eng = PaneEngine::spawn_pty(
        GridSize::new(100, 30, 9, 18),
        None,
        Some((fish.display().to_string(), Vec::new())),
        4242,
        10_000,
    )
    .expect("spawn fish");

    let pump = |eng: &mut PaneEngine, ms: u64| {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            let _ = eng.drain_pty();
            thread::sleep(Duration::from_millis(10));
        }
    };
    pump(&mut eng, 1500);

    // fish must have sent capability queries and received answers. This is the
    // assertion that discriminates: some fish versions time out their queries
    // and proceed anyway, so "fish ran a command" alone would pass even with
    // the responses disabled.
    assert!(
        eng.query_replies_sent() > 0,
        "fish startup should have queried the terminal and been answered; \
         got {} replies. If this is 0, fish sent no queries or we answered none.",
        eng.query_replies_sent()
    );
    eprintln!(
        "answered {} capability queries during startup",
        eng.query_replies_sent()
    );

    // The marker must appear only in command OUTPUT, not in the echoed input,
    // otherwise a blocked fish that merely echoes keystrokes would pass.
    eng.write_to_pty(b"printf 'fish-%s\\n' unblocked\n")
        .expect("write command");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut found = false;
    while Instant::now() < deadline {
        let _ = eng.drain_pty();
        if eng.screen_text().contains("fish-unblocked") {
            found = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        found,
        "fish must execute a command instead of blocking on queries; screen was:\n{}",
        eng.screen_text()
    );
}
