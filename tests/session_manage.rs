//! Creating and renaming multiplexer sessions, against real tmux on a private
//! socket so the developer's live sessions are never touched.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mutiny::app::AppController;
use mutiny::config::Overrides;
use mutiny::mux::{MuxKind, SessionMode};

fn find_tmux() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MUTINY_TEST_TMUX") {
        let p = PathBuf::from(p);
        return if p.is_file() { Some(p) } else { None };
    }
    let out = std::process::Command::new("sh")
        .args(["-c", "command -v tmux"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() {
        None
    } else {
        Some(PathBuf::from(p))
    }
}

struct Env {
    dir: PathBuf,
    tmux: PathBuf,
    sock: PathBuf,
}

impl Env {
    fn new(tag: &str, tmux: PathBuf, preferred: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mutiny-manage-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("tmux.sock");
        std::fs::write(
            dir.join("config.toml"),
            format!(
                "[sessions]\ntmux_program = {:?}\ntmux_socket = {:?}\nzellij_program = \"/nonexistent-zellij\"\nssh_program = \"/nonexistent-ssh\"\npreferred = {:?}\nrefresh_interval_s = 0\n",
                tmux.display().to_string(), sock.display().to_string(), preferred
            ),
        ).unwrap();
        Self { dir, tmux, sock }
    }
    fn tmux(&self, args: &[&str]) -> std::process::Output {
        std::process::Command::new(&self.tmux)
            .arg("-S")
            .arg(&self.sock)
            .args(args)
            .output()
            .expect("tmux")
    }
    fn sessions(&self) -> Vec<String> {
        let out = self.tmux(&["list-sessions", "-F", "#{session_name}"]);
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }
    fn controller(&self) -> Rc<AppController> {
        AppController::with_config(
            Some((String::from("/bin/sh"), Vec::new())),
            self.dir.join("audit.jsonl"),
            self.dir.join("config.toml"),
            Overrides::default(),
        )
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn pump(ctrl: &Rc<AppController>, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        ctrl.poll_once();
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn rename_renames_the_real_session_and_every_pane_follows() {
    let Some(tmux) = find_tmux() else {
        eprintln!("skipping: tmux not installed (CI installs it)");
        return;
    };
    let env = Env::new("rename", tmux, "tmux");
    assert!(
        env.tmux(&["new-session", "-d", "-s", "before"])
            .status
            .success()
    );
    let ctrl = env.controller();
    ctrl.create_window(None).expect("window");
    // Two panes on the SAME session: both must follow the rename.
    let (_w, _t, p1) = ctrl
        .open_session(MuxKind::Tmux, "before", None, SessionMode::Tab, None, None)
        .expect("open 1");
    let (_w2, _t2, p2) = ctrl
        .open_session(MuxKind::Tmux, "before", None, SessionMode::Tab, None, None)
        .expect("open 2");
    pump(&ctrl, 1200);

    ctrl.rename_session(MuxKind::Tmux, "before", "after", None)
        .expect("rename");
    pump(&ctrl, 1500);

    let names = env.sessions();
    assert!(
        names.contains(&"after".to_string()),
        "tmux must really be renamed: {names:?}"
    );
    assert!(
        !names.contains(&"before".to_string()),
        "the old name must be gone: {names:?}"
    );
    for pid in [p1, p2] {
        let ws = ctrl.workspace.borrow();
        let eng = ws.pane(pid).unwrap();
        assert_eq!(
            eng.mux_session().unwrap().name,
            "after",
            "pane ref must follow"
        );
        assert_eq!(eng.title(), "after", "the tab label derives from the ref");
    }
    ctrl.shutdown_ipc();
}

#[test]
fn a_failed_rename_does_not_claim_the_new_name() {
    let Some(tmux) = find_tmux() else {
        eprintln!("skipping: tmux not installed");
        return;
    };
    let env = Env::new("renamefail", tmux, "tmux");
    assert!(
        env.tmux(&["new-session", "-d", "-s", "keep"])
            .status
            .success()
    );
    let ctrl = env.controller();
    ctrl.create_window(None).expect("window");
    let (_w, _t, p) = ctrl
        .open_session(MuxKind::Tmux, "keep", None, SessionMode::Tab, None, None)
        .expect("open");
    pump(&ctrl, 1000);
    // Renaming a session that does not exist must fail without touching the
    // pane that is attached to a different one.
    ctrl.rename_session(MuxKind::Tmux, "nonexistent", "whatever", None)
        .expect("launch");
    pump(&ctrl, 1200);
    assert_eq!(
        ctrl.workspace
            .borrow()
            .pane(p)
            .unwrap()
            .mux_session()
            .unwrap()
            .name,
        "keep",
        "a failed rename must not rewrite unrelated panes"
    );
    assert!(env.sessions().contains(&"keep".to_string()));
    ctrl.shutdown_ipc();
}

#[test]
fn new_session_creates_and_refuses_to_silently_attach_on_a_collision() {
    let Some(tmux) = find_tmux() else {
        eprintln!("skipping: tmux not installed");
        return;
    };
    let env = Env::new("create", tmux, "tmux");
    assert!(
        env.tmux(&["new-session", "-d", "-s", "taken"])
            .status
            .success()
    );
    let ctrl = env.controller();
    ctrl.create_window(None).expect("window");

    // A fresh name creates a real session.
    ctrl.new_session(
        Some(MuxKind::Tmux),
        Some("brand-new".into()),
        SessionMode::Tab,
        None,
    )
    .expect("create");
    pump(&ctrl, 1500);
    assert!(
        env.sessions().contains(&"brand-new".to_string()),
        "{:?}",
        env.sessions()
    );

    // The same name again must NOT attach to the existing session. tmux
    // new-session without -A fails, so the pane's client dies and no second
    // client appears on "taken".
    assert!(
        env.tmux(&["new-session", "-d", "-s", "taken2"])
            .status
            .success()
    );
    ctrl.new_session(
        Some(MuxKind::Tmux),
        Some("taken".into()),
        SessionMode::Tab,
        None,
    )
    .expect("launch");
    pump(&ctrl, 1500);
    let after = env.tmux(&["list-clients", "-F", "#{client_session}"]);
    let text = String::from_utf8_lossy(&after.stdout).into_owned();
    let clients = text.lines().filter(|l| *l == "taken").count();
    assert_eq!(
        clients, 0,
        "create must not attach to an existing session: {text}"
    );
    ctrl.shutdown_ipc();
}

#[test]
fn new_tab_follows_the_preferred_multiplexer_and_spawn_tab_stays_plain() {
    let Some(tmux) = find_tmux() else {
        eprintln!("skipping: tmux not installed");
        return;
    };
    // preferred = tmux: the New Tab ACTION creates a session.
    let env = Env::new("pref-tmux", tmux, "tmux");
    let ctrl = env.controller();
    ctrl.create_window(None).expect("window");
    ctrl.execute_action(mutiny::action::ActionId::NewTab, None);
    pump(&ctrl, 1500);
    let created: Vec<String> = env.sessions();
    assert!(
        created.iter().any(|n| n.starts_with("mutiny-")),
        "New Tab must create an auto-named session: {created:?}"
    );
    // The explicit IPC verb stays a plain shell, so scripts do not change.
    let (_w, _t, plain) = ctrl.create_tab(None, None).expect("spawn-tab");
    assert!(
        ctrl.workspace
            .borrow()
            .pane(plain)
            .unwrap()
            .mux_session()
            .is_none(),
        "SpawnTab must stay a plain shell"
    );
    ctrl.shutdown_ipc();
    drop(env);

    // preferred = none: New Tab is a plain shell again.
    let env = Env::new("pref-none", find_tmux().unwrap(), "none");
    let ctrl = env.controller();
    ctrl.create_window(None).expect("window");
    ctrl.execute_action(mutiny::action::ActionId::NewTab, None);
    pump(&ctrl, 800);
    let attached = ctrl
        .workspace
        .borrow()
        .windows()
        .iter()
        .flat_map(|w| w.tabs.iter())
        .filter_map(|t| {
            ctrl.workspace
                .borrow()
                .pane(t.active_pane)
                .and_then(|e| e.mux_session().cloned())
        })
        .count();
    assert_eq!(
        attached, 0,
        "preferred = none must leave New Tab a plain shell"
    );
    assert!(
        env.sessions().is_empty(),
        "no session should have been created"
    );
    ctrl.shutdown_ipc();
}

/// The distinction Ian asked to reconcile: closing a tab ends only this client
/// and the session keeps running; ending a session stops it. Both must be true,
/// or the menu wording is a lie.
#[test]
fn closing_a_tab_keeps_the_session_but_ending_it_stops_it() {
    let Some(tmux) = find_tmux() else {
        eprintln!("skipping: tmux not installed (CI installs it)");
        return;
    };
    let env = Env::new("close-vs-end", tmux, "none");
    assert!(
        env.tmux(&["new-session", "-d", "-s", "persist"])
            .status
            .success()
    );
    assert!(
        env.tmux(&["new-session", "-d", "-s", "doomed"])
            .status
            .success()
    );
    let ctrl = env.controller();
    let (w, _t, _p) = ctrl.create_window(None).expect("window");
    let (_w, tab_keep, _pk) = ctrl
        .open_session(
            MuxKind::Tmux,
            "persist",
            None,
            SessionMode::Tab,
            Some(w),
            None,
        )
        .expect("open persist");
    let (_w2, _t2, _pd) = ctrl
        .open_session(
            MuxKind::Tmux,
            "doomed",
            None,
            SessionMode::Tab,
            Some(w),
            None,
        )
        .expect("open doomed");
    pump(&ctrl, 1500);
    assert!(env.sessions().contains(&"persist".to_string()));
    assert!(env.sessions().contains(&"doomed".to_string()));

    // Closing the tab must NOT end the session.
    assert!(ctrl.close_tab(w, tab_keep), "close_tab must report success");
    pump(&ctrl, 1200);
    assert!(
        env.sessions().contains(&"persist".to_string()),
        "closing a tab must leave the session running: {:?}",
        env.sessions()
    );

    // Ending the session must actually stop it.
    ctrl.end_session(MuxKind::Tmux, "doomed", None)
        .expect("end");
    pump(&ctrl, 1500);
    assert!(
        !env.sessions().contains(&"doomed".to_string()),
        "ending a session must stop it: {:?}",
        env.sessions()
    );
    assert!(
        env.sessions().contains(&"persist".to_string()),
        "ending one session must not touch another"
    );
    ctrl.shutdown_ipc();
}
