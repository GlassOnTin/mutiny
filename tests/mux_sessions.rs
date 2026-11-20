//! Session discovery and attach, end to end.
//!
//! Every tmux command here runs on a PRIVATE `-S` socket created per test and
//! killed on teardown. The developer's real sessions are never listed, attached,
//! or resized: tmux's `window-size latest` means attaching to a live session
//! would resize every other viewer, including a phone.
//!
//! Remote behaviour is exercised with a fake `ssh` shim that reproduces the one
//! thing that matters about real ssh: it hands the command string to a login
//! shell. That makes the double-parse hazard testable without a second machine.

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use mutiny::app::AppController;
use mutiny::config::Overrides;
use mutiny::mux::{HostState, MuxKind, SessionMode};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mutiny-mux-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Same shape as the fish probe: explicit override first, then PATH, then skip.
fn find_tmux() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("MUTINY_TEST_TMUX") {
        let path = PathBuf::from(&explicit);
        return if path.is_file() { Some(path) } else { None };
    }
    let out = std::process::Command::new("sh")
        .args(["-c", "command -v tmux"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// A controller whose `[sessions]` table points at a private tmux socket.
fn controller(dir: &TempDir, sessions_toml: &str) -> Rc<AppController> {
    let cfg = dir.path("config.toml");
    fs::write(&cfg, format!("[sessions]\n{sessions_toml}\n")).unwrap();
    AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.path("audit.jsonl"),
        cfg,
        Overrides::default(),
    )
}

fn pump(ctrl: &Rc<AppController>, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(5));
    }
}

/// Runs a discovery pass to completion, with a bound so a hang fails the test
/// rather than the whole suite.
fn discover(ctrl: &Rc<AppController>) -> mutiny::mux::MuxInventory {
    ctrl.begin_discovery(vec![None], true, None);
    let end = Instant::now() + Duration::from_secs(20);
    while ctrl.discovery_in_flight() && Instant::now() < end {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!ctrl.discovery_in_flight(), "discovery did not finish");
    ctrl.session_inventory()
}

struct PrivateTmux {
    program: PathBuf,
    socket: PathBuf,
}

impl PrivateTmux {
    fn new(dir: &TempDir, program: PathBuf) -> Self {
        Self {
            program,
            socket: dir.path("tmux.sock"),
        }
    }
    fn run(&self, args: &[&str]) -> std::process::Output {
        std::process::Command::new(&self.program)
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .expect("tmux")
    }
    fn config_line(&self) -> String {
        format!(
            "tmux_program = {:?}\ntmux_socket = {:?}\nzellij_program = \"/nonexistent-zellij\"",
            self.program.display().to_string(),
            self.socket.display().to_string()
        )
    }
}

impl Drop for PrivateTmux {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
    }
}

#[test]
fn discovery_lists_a_session_on_a_private_socket() {
    let Some(tmux) = find_tmux() else {
        eprintln!("skipping: tmux not installed (CI installs it)");
        return;
    };
    let dir = TempDir::new("discover");
    let private = PrivateTmux::new(&dir, tmux);
    assert!(
        private
            .run(&["new-session", "-d", "-s", "probe-alpha"])
            .status
            .success()
    );

    let ctrl = controller(&dir, &private.config_line());
    let inventory = discover(&ctrl);
    let found = inventory
        .sessions
        .iter()
        .find(|s| s.name == "probe-alpha")
        .unwrap_or_else(|| panic!("session not discovered: {inventory:?}"));
    assert_eq!(found.kind, MuxKind::Tmux);
    assert_eq!(found.host, None);
    assert_eq!(found.attached, 0, "nobody is attached yet");
    assert!(!found.exited);
    let status = inventory
        .hosts
        .iter()
        .find(|h| h.host.is_none() && h.kind == MuxKind::Tmux)
        .expect("tmux host status");
    assert_eq!(status.state, HostState::Ok);
}

#[test]
fn a_missing_multiplexer_is_reported_not_fatal() {
    let dir = TempDir::new("missing");
    let ctrl = controller(
        &dir,
        "tmux_program = \"/nonexistent-tmux\"\nzellij_program = \"/nonexistent-zellij\"",
    );
    let inventory = discover(&ctrl);
    assert!(inventory.sessions.is_empty());
    for host in &inventory.hosts {
        assert_eq!(host.state, HostState::Error);
        assert!(
            host.error
                .as_deref()
                .unwrap_or("")
                .contains("not installed"),
            "the remedy must be named: {host:?}"
        );
    }
}

/// The user's stated decision: share, never detach. Two mutiny panes attached to
/// one session must both stay connected.
#[test]
fn attaching_shares_the_session_and_never_detaches_other_clients() {
    let Some(tmux) = find_tmux() else {
        eprintln!("skipping: tmux not installed (CI installs it)");
        return;
    };
    let dir = TempDir::new("share");
    let private = PrivateTmux::new(&dir, tmux);
    assert!(
        private
            .run(&["new-session", "-d", "-s", "shared"])
            .status
            .success()
    );

    let ctrl = controller(&dir, &private.config_line());
    ctrl.create_window(None).expect("window");
    for mode in [SessionMode::Tab, SessionMode::Window] {
        ctrl.open_session(MuxKind::Tmux, "shared", None, mode, None, None)
            .expect("open session");
    }
    pump(&ctrl, 1_500);

    let clients = private.run(&["list-clients", "-F", "#{client_session}"]);
    let text = String::from_utf8_lossy(&clients.stdout);
    let count = text.lines().filter(|l| *l == "shared").count();
    assert_eq!(
        count, 2,
        "both clients must remain attached (an attach -d/-D implementation fails here):\n{text}"
    );
}

/// tmux re-parses a trailing `;` in its own argv even with no shell involved, so
/// a name ending in one needs escaping or the attach silently targets the wrong
/// session.
#[test]
fn a_session_name_ending_in_a_semicolon_attaches_to_the_right_session() {
    let Some(tmux) = find_tmux() else {
        eprintln!("skipping: tmux not installed (CI installs it)");
        return;
    };
    let dir = TempDir::new("semicolon");
    let private = PrivateTmux::new(&dir, tmux);
    // Create the session with the escaped name, so it is literally `probe;`.
    assert!(
        private
            .run(&["new-session", "-d", "-s", "probe\\;"])
            .status
            .success()
    );
    let listed = private.run(&["list-sessions", "-F", "#{session_name}"]);
    assert!(
        String::from_utf8_lossy(&listed.stdout)
            .lines()
            .any(|l| l == "probe;"),
        "fixture must really be named 'probe;': {}",
        String::from_utf8_lossy(&listed.stdout)
    );
    // A decoy that an unescaped attach would land on instead.
    assert!(
        private
            .run(&["new-session", "-d", "-s", "probe"])
            .status
            .success()
    );

    let ctrl = controller(&dir, &private.config_line());
    ctrl.create_window(None).expect("window");
    ctrl.open_session(MuxKind::Tmux, "probe;", None, SessionMode::Tab, None, None)
        .expect("open session");
    pump(&ctrl, 1_500);

    let clients = private.run(&["list-clients", "-F", "#{client_session}"]);
    let text = String::from_utf8_lossy(&clients.stdout);
    assert!(
        text.lines().any(|l| l == "probe;"),
        "must attach to 'probe;', not the decoy 'probe':\n{text}"
    );
}

/// The single most discriminating test for the async design: a multiplexer that
/// hangs must not stall the GUI tick.
#[test]
fn a_hanging_multiplexer_does_not_block_the_tick() {
    let dir = TempDir::new("tick");
    let slow = dir.path("slow-tmux");
    write_executable(&slow, "#!/bin/sh\nsleep 5\n");
    let ctrl = controller(
        &dir,
        &format!(
            "tmux_program = {:?}\nzellij_program = \"/bin/true\"",
            slow.display().to_string()
        ),
    );

    ctrl.begin_discovery(vec![None], true, None);
    let started = Instant::now();
    ctrl.poll_once();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(50),
        "poll_once blocked for {elapsed:?}; a synchronous .output() would take 5 s"
    );
    assert!(
        ctrl.discovery_in_flight(),
        "the pass must still be running, not completed synchronously"
    );
    ctrl.shutdown_ipc();
}

#[test]
fn a_hanging_discovery_times_out_and_is_reaped() {
    let dir = TempDir::new("deadline");
    let slow = dir.path("slow-tmux");
    write_executable(&slow, "#!/bin/sh\nsleep 30\n");
    let ctrl = controller(
        &dir,
        &format!(
            "tmux_program = {:?}\nzellij_program = \"/bin/true\"\ndiscovery_timeout_ms = 300",
            slow.display().to_string()
        ),
    );

    let inventory = discover(&ctrl);
    let tmux_status = inventory
        .hosts
        .iter()
        .find(|h| h.kind == MuxKind::Tmux)
        .expect("tmux status");
    assert_eq!(tmux_status.state, HostState::Error);
    assert!(
        tmux_status
            .error
            .as_deref()
            .unwrap_or("")
            .contains("timed out"),
        "expected a timeout, got {tmux_status:?}"
    );
    // Jobs are only removed once try_wait reaps them, so an empty queue is the
    // observable proof the child was killed AND reaped rather than leaked.
    assert!(!ctrl.discovery_in_flight());
    ctrl.shutdown_ipc();
}

/// Remote quoting, without a second machine. The shim does what real ssh does:
/// it hands the command string to a login shell. A fake tmux records the argv it
/// actually received, so an injection would show up as an extra argument.
#[test]
fn a_hostile_session_name_reaches_the_remote_program_as_one_argument() {
    let dir = TempDir::new("remote");
    let ssh_log = dir.path("ssh-argv.log");
    let tmux_log = dir.path("tmux-argv.log");

    let fake_tmux = dir.path("fake-tmux");
    write_executable(
        &fake_tmux,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {:?}\n",
            tmux_log.display().to_string()
        ),
    );
    let fake_ssh = dir.path("fake-ssh");
    write_executable(
        &fake_ssh,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {:?}\nfor last; do :; done\nexec /bin/sh -c \"$last\"\n",
            ssh_log.display().to_string()
        ),
    );

    let ctrl = controller(
        &dir,
        &format!(
            "ssh_program = {:?}\ntmux_program = {:?}\nzellij_program = \"/nonexistent-zellij\"",
            fake_ssh.display().to_string(),
            fake_tmux.display().to_string()
        ),
    );
    ctrl.create_window(None).expect("window");
    ctrl.open_session(
        MuxKind::Tmux,
        "a;echo INJECTED",
        Some("probe-host"),
        SessionMode::Tab,
        None,
        None,
    )
    .expect("open remote session");
    pump(&ctrl, 1_500);

    let ssh_argv = fs::read_to_string(&ssh_log).expect("ssh shim ran");
    assert!(
        ssh_argv.lines().any(|l| l == "--"),
        "the host must be guarded by --:\n{ssh_argv}"
    );
    assert!(
        ssh_argv.lines().any(|l| l == "-t"),
        "attach needs a tty:\n{ssh_argv}"
    );
    assert!(
        ssh_argv.lines().any(|l| l == "probe-host"),
        "host must be passed:\n{ssh_argv}"
    );

    let tmux_argv = fs::read_to_string(&tmux_log).expect("inner program ran");
    assert!(
        tmux_argv.lines().any(|l| l == "a;echo INJECTED"),
        "the name must arrive as ONE argument:\n{tmux_argv}"
    );
    assert!(
        !tmux_argv.lines().any(|l| l == "INJECTED"),
        "the injected command must not have run:\n{tmux_argv}"
    );
}

/// Remote discovery must never prompt, and must not allocate a tty.
#[test]
fn remote_discovery_uses_batchmode_and_no_tty() {
    let dir = TempDir::new("remote-discover");
    let ssh_log = dir.path("ssh-argv.log");
    let fake_ssh = dir.path("fake-ssh");
    write_executable(
        &fake_ssh,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {:?}\nexit 1\n",
            ssh_log.display().to_string()
        ),
    );
    let ctrl = controller(
        &dir,
        &format!(
            "ssh_program = {:?}\nzellij_program = \"/nonexistent-zellij\"",
            fake_ssh.display().to_string()
        ),
    );
    ctrl.begin_discovery(vec![Some(String::from("probe-host"))], true, None);
    let end = Instant::now() + Duration::from_secs(10);
    while ctrl.discovery_in_flight() && Instant::now() < end {
        ctrl.poll_once();
        thread::sleep(Duration::from_millis(5));
    }
    let argv = fs::read_to_string(&ssh_log).expect("ssh shim ran");
    assert!(argv.lines().any(|l| l == "BatchMode=yes"), "{argv}");
    assert!(argv.lines().any(|l| l == "-n"), "{argv}");
    assert!(
        !argv.lines().any(|l| l == "-t"),
        "discovery needs no tty:\n{argv}"
    );
    assert!(argv.lines().any(|l| l == "--"), "{argv}");
}

/// A hostile host must be refused before any process is spawned.
#[test]
fn an_option_injection_host_is_refused_without_spawning() {
    let dir = TempDir::new("badhost");
    let marker = dir.path("spawned");
    let fake_ssh = dir.path("fake-ssh");
    write_executable(
        &fake_ssh,
        &format!("#!/bin/sh\ntouch {:?}\n", marker.display().to_string()),
    );
    let ctrl = controller(
        &dir,
        &format!(
            "ssh_program = {:?}\nzellij_program = \"/nonexistent-zellij\"",
            fake_ssh.display().to_string()
        ),
    );
    let inventory_err = ctrl.open_session(
        MuxKind::Tmux,
        "x",
        Some("-oProxyCommand=touch /tmp/pwn"),
        SessionMode::Tab,
        None,
        None,
    );
    assert!(
        inventory_err.is_err(),
        "an option-shaped host must be refused"
    );
    pump(&ctrl, 200);
    assert!(!marker.exists(), "no process may be spawned for a bad host");
}

/// The pane label must survive tmux's title handling, which otherwise leaves the
/// tab reading "shell".
#[test]
fn an_attached_pane_is_labelled_with_its_session() {
    let Some(tmux) = find_tmux() else {
        eprintln!("skipping: tmux not installed (CI installs it)");
        return;
    };
    let dir = TempDir::new("label");
    let private = PrivateTmux::new(&dir, tmux);
    assert!(
        private
            .run(&["new-session", "-d", "-s", "labelled"])
            .status
            .success()
    );
    let ctrl = controller(&dir, &private.config_line());
    ctrl.create_window(None).expect("window");
    let (_w, _t, pane) = ctrl
        .open_session(
            MuxKind::Tmux,
            "labelled",
            None,
            SessionMode::Tab,
            None,
            None,
        )
        .expect("open session");
    pump(&ctrl, 800);
    let ws = ctrl.workspace.borrow();
    let eng = ws.pane(pane).expect("pane");
    // Haven sibling convention: title is the session name, subtitle the host.
    assert_eq!(eng.title(), "labelled", "title must be the session name");
    let session = eng.mux_session().expect("session ref pinned");
    assert_eq!(session.kind, MuxKind::Tmux);
    assert_eq!(session.name, "labelled");
    assert_eq!(session.host, None);
    assert_eq!(
        session.subtitle(),
        mutiny::mux::local_hostname(),
        "a local session's subtitle is this hostname"
    );
    assert!(!session.subtitle().is_empty());
}
