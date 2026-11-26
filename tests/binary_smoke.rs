//! End-to-end smoke test against the REAL binary: launch, act, save, quit,
//! relaunch, and check the layout came back.
//!
//! Why this exists: every other GUI test constructs widgets directly, which
//! bypasses the real startup sequence. Session restore was fully covered by
//! headless tests and still completely broken in the GUI, because restore ran
//! before the application was attached and no test drove the actual binary.
//! This is the path users take, so it is the path that needs a guard.
//!
//! Needs a display for the child process; CI runs the suite under Xvfb. Skips
//! when DISPLAY is empty, which is the displayless job.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_mutiny");

struct Guard(Option<Child>);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct Harness {
    dir: PathBuf,
    sock: PathBuf,
    child: Guard,
}

impl Harness {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mutiny-smoke-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("xdg/mutiny")).unwrap();
        std::fs::create_dir_all(dir.join("state")).unwrap();
        // A fake tmux with a fixed listing, so the smoke test is deterministic
        // and never touches the developer's real sessions.
        let fake = dir.join("fake-tmux");
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf 'alpha\\t1\\t0\\t1\\t/home/x\\nbeta\\t2\\t1\\t2\\t/home/y\\n'\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            dir.join("xdg/mutiny/config.toml"),
            format!(
                "[sessions]\ntmux_program = {:?}\nzellij_program = \"/nonexistent-zellij\"\nssh_program = \"/nonexistent-ssh\"\nrefresh_interval_s = 0\n",
                fake.display().to_string()
            ),
        )
        .unwrap();
        Self {
            sock: dir.join("m.sock"),
            dir,
            child: Guard(None),
        }
    }

    fn launch(&mut self, extra: &[&str]) {
        let mut cmd = Command::new(BIN);
        cmd.args(extra)
            .env("MUTINY_SOCKET", &self.sock)
            .env("XDG_CONFIG_HOME", self.dir.join("xdg"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("WAYLAND_DISPLAY", "")
            .env("GDK_BACKEND", "x11")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("launch mutiny");
        self.child = Guard(Some(child));
        // Wait for the control socket, which is the real readiness signal.
        let end = Instant::now() + Duration::from_secs(25);
        while Instant::now() < end {
            if self.sock.exists() && self.ctl(&["list"]).is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        panic!("mutiny did not come up within 25 s");
    }

    fn ctl(&self, args: &[&str]) -> Option<serde_json::Value> {
        let out = Command::new(BIN)
            .arg("--socket")
            .arg(&self.sock)
            .arg("ctl")
            .args(args)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        serde_json::from_slice(&out.stdout).ok()
    }

    fn layout(&self) -> Vec<(u64, Vec<u64>)> {
        let v = self.ctl(&["list"]).expect("list");
        v["workspace"]["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| {
                (
                    w["id"].as_u64().unwrap(),
                    w["tabs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|t| t["id"].as_u64().unwrap())
                        .collect(),
                )
            })
            .collect()
    }

    fn wait_exit(&mut self) {
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            if let Some(child) = self.child.0.as_mut()
                && matches!(child.try_wait(), Ok(Some(_)))
            {
                self.child.0 = None;
                return;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        panic!("mutiny did not exit after quit");
    }

    fn session_file(&self) -> PathBuf {
        self.dir.join("state/mutiny/session.json")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn display_available() -> bool {
    std::env::var("DISPLAY")
        .map(|d| !d.is_empty())
        .unwrap_or(false)
}

#[test]
fn real_binary_launches_acts_saves_quits_and_reopens() {
    if !display_available() {
        eprintln!("skipping: no DISPLAY (CI runs this under Xvfb)");
        return;
    }
    let mut h = Harness::new("cycle");

    // --- launch, with restore explicitly off so we start clean ---
    h.launch(&["--no-restore"]);
    let initial = h.layout();
    assert_eq!(initial.len(), 1, "one window on a fresh start: {initial:?}");
    assert_eq!(initial[0].1.len(), 1, "one tab on a fresh start");

    // --- act: add a tab and a second window ---
    assert!(h.ctl(&["spawn-tab"]).is_some(), "spawn-tab must work");
    assert!(h.ctl(&["spawn-window"]).is_some(), "spawn-window must work");
    let grown = h.layout();
    assert_eq!(grown.len(), 2, "two windows after spawn-window: {grown:?}");
    assert_eq!(grown[0].1.len(), 2, "two tabs in the first window");

    // --- close verbs, the counterparts to the spawn verbs. Targets are
    // explicit: the defaults follow the ACTIVE window, which after
    // spawn-window is the new one, not the first.
    let first_win = grown[0].0.to_string();
    let second_win = grown[1].0.to_string();
    assert!(
        h.ctl(&["close-tab", "--window", &first_win]).is_some(),
        "close-tab must work"
    );
    let after_tab = h.layout();
    assert_eq!(
        after_tab[0].1.len(),
        1,
        "close-tab must remove one tab from the named window: {after_tab:?}"
    );
    assert_eq!(after_tab.len(), 2, "the other window must be untouched");
    assert!(
        h.ctl(&["close-window", "--window", &second_win]).is_some(),
        "close-window must work"
    );
    let after_win = h.layout();
    assert_eq!(
        after_win.len(),
        1,
        "close-window must remove a window: {after_win:?}"
    );
    assert_eq!(
        after_win[0].0, grown[0].0,
        "the named window went, not the other"
    );
    // Put it back so the save/restore comparison below has two windows.
    assert!(h.ctl(&["spawn-tab"]).is_some());
    assert!(h.ctl(&["spawn-window"]).is_some());
    let built = h.layout();
    assert_eq!(built.len(), 2);
    assert_eq!(built[0].1.len(), 2);

    // --- save on demand, then quit ---
    assert!(h.ctl(&["save-session"]).is_some(), "save-session must work");
    assert!(
        h.session_file().is_file(),
        "save-session must write the bundle"
    );
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(h.session_file()).unwrap()).unwrap();
    assert_eq!(saved["windows"].as_array().unwrap().len(), 2);

    assert!(h.ctl(&["quit"]).is_some(), "quit must reply before exiting");
    h.wait_exit();
    assert!(
        h.session_file().is_file(),
        "quit must leave the bundle behind"
    );

    // --- relaunch with restore on: the layout must come back ---
    h.launch(&[]);
    let reopened = h.layout();
    assert_eq!(
        reopened, built,
        "the real binary must reopen the saved arrangement: {reopened:?} vs {built:?}"
    );

    // And the windows must be REAL, not just model entries: a snapshot of a
    // pane proves a live view and PTY exist.
    let snap = h.ctl(&["snapshot"]).expect("snapshot");
    assert_eq!(snap["status"], "ok", "a restored pane must be live: {snap}");

    h.ctl(&["quit"]);
    h.wait_exit();
}

/// The Sessions submenu path must work in the real binary too, since it is
/// driven by the tick rather than by any test calling the controller directly.
#[test]
fn real_binary_lists_sessions_without_a_picker() {
    if !display_available() {
        eprintln!("skipping: no DISPLAY (CI runs this under Xvfb)");
        return;
    }
    let mut h = Harness::new("sessions");
    h.launch(&["--no-restore"]);
    let end = Instant::now() + Duration::from_secs(15);
    let mut names = Vec::new();
    while Instant::now() < end {
        if let Some(v) = h.ctl(&["sessions"]) {
            names = v["sessions"]["sessions"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|s| s["name"].as_str().unwrap_or("").to_owned())
                        .collect()
                })
                .unwrap_or_default();
            if !names.is_empty() {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        names.contains(&"alpha".to_string()) && names.contains(&"beta".to_string()),
        "the real binary must discover sessions on its own: {names:?}"
    );
    h.ctl(&["quit"]);
    h.wait_exit();
}

/// Every IPC verb name must also work as a ctl subcommand. Five verbs had a
/// shorter CLI name than their wire name (audit vs audit-log, run vs run-action,
/// sessions vs list-sessions, screen vs screen-text, wait vs wait-prompt), which
/// is a papercut for anyone reading the IPC surface and typing it at the CLI.
/// Aliases keep both working without breaking the existing short names.
#[test]
fn every_ipc_verb_name_is_accepted_by_the_cli() {
    for name in [
        "list",
        "actions",
        "audit-log",
        "snapshot",
        "read-exited",
        "search",
        "run-action",
        "spawn-window",
        "spawn-tab",
        "split",
        "focus",
        "detach-split",
        "move-pane",
        "send-text",
        "send-input",
        "send-to-agent",
        "present-notice",
        "last-output",
        "screen-text",
        "wait-prompt",
        "await-turn",
        "present-media",
        "list-sessions",
        "open-session",
        "save-session",
        "close-pane",
        "close-tab",
        "close-window",
        "quit",
        "list-peers",
        "list-peer-alerts",
        "clear-peer-alerts",
        "pair-haven",
        "enroll-haven-signer",
        "revoke-peer",
    ] {
        let out = Command::new(BIN)
            .arg("ctl")
            .arg(name)
            .arg("--help")
            .output()
            .expect("run ctl");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "ctl {name} is not a recognised subcommand: {stderr}"
        );
    }
}

/// The natural quit path: closing the last WINDOW, not `ctl quit`. The exit save
/// runs after the model has been emptied, so this path used to wipe the bundle
/// and restore had nothing to reopen. It is how people actually quit.
#[test]
fn closing_the_last_window_preserves_the_layout_for_next_launch() {
    if !display_available() {
        eprintln!("skipping: no DISPLAY (CI runs this under Xvfb)");
        return;
    }
    let mut h = Harness::new("lastwin");
    h.launch(&["--no-restore"]);
    assert!(h.ctl(&["spawn-tab"]).is_some());
    assert!(h.ctl(&["spawn-tab"]).is_some());
    let built = h.layout();
    assert_eq!(built[0].1.len(), 3, "three tabs: {built:?}");

    // Close the only window. The app should save, then exit.
    assert!(
        h.ctl(&["close-window"]).is_some(),
        "close-window must reply"
    );
    h.wait_exit();

    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(h.session_file()).unwrap()).unwrap();
    assert_eq!(
        saved["windows"].as_array().unwrap().len(),
        1,
        "closing the last window must not wipe the bundle: {saved}"
    );
    assert_eq!(saved["panes"].as_array().unwrap().len(), 3);

    // And the next launch really reopens it.
    h.launch(&[]);
    assert_eq!(
        h.layout(),
        built,
        "the layout must come back after a natural quit"
    );
    h.ctl(&["quit"]);
    h.wait_exit();
}
