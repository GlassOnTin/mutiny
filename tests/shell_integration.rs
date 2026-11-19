//! Proves the bundled shell integration loads automatically. This is its own
//! test binary with a single test because it sets process-wide environment
//! variables (HOME, SHELL, XDG_RUNTIME_DIR) to simulate a fresh install, and
//! that must not race with other tests in the same process.

use std::env;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use mutiny::engine::{GridSize, PaneEngine};

fn pump(engine: &mut PaneEngine, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        let _ = engine.drain_pty();
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn fresh_plain_bash_emits_osc133_and_osc7_without_manual_setup() {
    let bash = ["/bin/bash", "/usr/bin/bash"]
        .iter()
        .find(|p| Path::new(p).is_file())
        .expect("bash must be present to run this test");

    let root = env::temp_dir().join(format!("mutiny-fresh-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let home = root.join("home");
    let runtime = root.join("runtime");
    fs::create_dir_all(&home).expect("home");
    fs::create_dir_all(&runtime).expect("runtime");

    // A fresh install: no dotfiles at all, and no manual rc configuration.
    unsafe {
        env::set_var("HOME", &home);
        env::set_var("SHELL", bash);
        env::set_var("XDG_RUNTIME_DIR", &runtime);
        env::remove_var("ZDOTDIR");
        env::remove_var("BASH_ENV");
        env::remove_var("ENV");
    }
    assert!(!home.join(".bashrc").exists(), "home must start empty");

    // No shell override: the engine must resolve $SHELL and inject on its own.
    let mut engine = PaneEngine::spawn_pty(GridSize::new(100, 30, 9, 18), None, None, 77, 10_000)
        .expect("spawn bash");

    pump(&mut engine, 1200);
    let startup_prompt = engine.prompt_seq();
    assert!(
        startup_prompt >= 1,
        "a fresh bash must emit OSC 133;A on its first prompt, got prompt_seq={startup_prompt}"
    );
    assert!(
        engine.cwd().is_some(),
        "OSC 7 must report the working directory"
    );

    // A successful command: output captured between C and D, exit code 0.
    engine
        .write_to_pty(b"echo mutiny-integration-ok\n")
        .expect("write echo");
    pump(&mut engine, 900);
    let echo_cmd = engine.last_command().cloned().expect("echo command record");
    assert_eq!(echo_cmd.exit_code, Some(0), "echo must report exit 0");
    assert!(
        echo_cmd.output.contains("mutiny-integration-ok"),
        "OSC 133 C/D must capture command output, got {:?}",
        echo_cmd.output
    );

    // A failing command: D carries the real exit code, which is what lights
    // up urgent-next and what await-turn's prompt path waits on.
    engine.write_to_pty(b"false\n").expect("write false");
    pump(&mut engine, 900);
    let false_cmd = engine
        .last_command()
        .cloned()
        .expect("false command record");
    assert_eq!(
        false_cmd.exit_code,
        Some(1),
        "OSC 133;D must carry the failing exit code"
    );
    assert!(
        engine.prompt_seq() > startup_prompt,
        "each command must be followed by a fresh prompt marker"
    );

    // The integration really was loaded, not something the host configured.
    engine
        .write_to_pty(b"echo loaded=$MUTINY_INTEGRATION_LOADED\n")
        .expect("write probe");
    pump(&mut engine, 900);
    let probe = engine.last_command().cloned().expect("probe record");
    assert!(
        probe.output.contains("loaded=1"),
        "snippet must have been sourced automatically, got {:?}",
        probe.output
    );

    // Assets landed in the runtime dir, and no dotfile was created.
    assert!(
        runtime.join("mutiny/shell-integration/bash.sh").is_file(),
        "snippet should be materialized under XDG_RUNTIME_DIR"
    );
    assert!(
        !home.join(".bashrc").exists(),
        "integration must not write to the user's dotfiles"
    );

    let _ = fs::remove_dir_all(&root);
}
