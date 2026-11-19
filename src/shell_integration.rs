//! Bundled shell integration: OSC 133 prompt/command markers and OSC 7 cwd.
//!
//! The snippets are compiled into the binary and written to a per-session
//! runtime directory when a pane spawns, so a fresh install gets prompt
//! jumping, command-output capture, and the `await-turn` prompt path without
//! manual setup. Nothing here edits the user's dotfiles: each shell is pointed
//! at a wrapper that loads the user's own config first, then the snippet.

use std::io;
use std::path::{Path, PathBuf};

pub const BASH: &str = include_str!("shell_integration/bash.sh");
pub const BASH_RC: &str = include_str!("shell_integration/bash_rc.sh");
pub const ZSH: &str = include_str!("shell_integration/zsh.sh");
pub const ZSH_ZSHENV: &str = include_str!("shell_integration/zsh_zshenv.sh");
pub const ZSH_ZPROFILE: &str = include_str!("shell_integration/zsh_zprofile.sh");
pub const ZSH_ZSHRC: &str = include_str!("shell_integration/zsh_zshrc.sh");
pub const FISH: &str = include_str!("shell_integration/fish.fish");
pub const NU: &str = include_str!("shell_integration/nu.nu");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
    Nushell,
    /// Not a shell we ship integration for (dash, sh, a custom program).
    Other,
}

/// Classifies by basename, tolerating a leading '-' that marks a login shell.
pub fn classify(program: &str) -> ShellKind {
    let base = Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program);
    match base.trim_start_matches('-') {
        "bash" => ShellKind::Bash,
        "zsh" => ShellKind::Zsh,
        "fish" => ShellKind::Fish,
        "nu" | "nushell" => ShellKind::Nushell,
        _ => ShellKind::Other,
    }
}

/// Per-session asset directory. Lives under the runtime dir so it is cleaned
/// up on logout, with a temp fallback when XDG_RUNTIME_DIR is unset.
pub fn integration_dir() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("mutiny").join("shell-integration")
}

/// Writes atomically so a shell starting concurrently never reads a torn file.
fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == contents) {
        return Ok(());
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// Materializes every snippet and wrapper. Idempotent.
pub fn install_assets(dir: &Path) -> io::Result<()> {
    write_atomic(&dir.join("bash.sh"), BASH)?;
    write_atomic(&dir.join("bash_rc.sh"), BASH_RC)?;
    write_atomic(&dir.join("zsh.sh"), ZSH)?;
    write_atomic(&dir.join("fish.fish"), FISH)?;
    write_atomic(&dir.join("nu.nu"), NU)?;
    let zdot = dir.join("zdot");
    write_atomic(&zdot.join(".zshenv"), ZSH_ZSHENV)?;
    write_atomic(&zdot.join(".zprofile"), ZSH_ZPROFILE)?;
    write_atomic(&zdot.join(".zshrc"), ZSH_ZSHRC)?;
    Ok(())
}

/// Extra argv and environment that point a shell at its integration snippet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Injection {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

fn nu_user_config(env_name: &str, file: &str) -> Option<PathBuf> {
    let base = std::env::var(env_name)
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    let path = base.join("nushell").join(file);
    path.is_file().then_some(path)
}

/// Builds the injection for a shell, or None when we ship nothing for it.
/// `existing_args` are the caller's own arguments and are preserved; an
/// explicit `--rcfile` or `--config` from the caller wins over ours.
pub fn prepare(
    kind: ShellKind,
    dir: &Path,
    existing_args: &[String],
) -> io::Result<Option<Injection>> {
    let has_flag = |flag: &str| existing_args.iter().any(|a| a == flag);
    let mut inj = Injection::default();
    inj.env.push((
        String::from("MUTINY_INTEGRATION_DIR"),
        dir.display().to_string(),
    ));

    match kind {
        ShellKind::Bash => {
            if has_flag("--rcfile") {
                return Ok(None);
            }
            inj.args.push(String::from("--rcfile"));
            inj.args.push(dir.join("bash_rc.sh").display().to_string());
        }
        ShellKind::Zsh => {
            let zdot = dir.join("zdot");
            // Preserve the user's ZDOTDIR so the wrappers can source their files.
            if let Ok(orig) = std::env::var("ZDOTDIR")
                && !orig.is_empty()
            {
                inj.env.push((String::from("MUTINY_ZDOTDIR_ORIG"), orig));
            }
            inj.env
                .push((String::from("ZDOTDIR"), zdot.display().to_string()));
        }
        ShellKind::Fish => {
            if has_flag("-C") || has_flag("--init-command") {
                return Ok(None);
            }
            inj.args.push(String::from("-C"));
            inj.args
                .push(format!("source {}", dir.join("fish.fish").display()));
        }
        ShellKind::Nushell => {
            if has_flag("--config") {
                return Ok(None);
            }
            // nu's `source` takes a literal path, so the wrapper is generated
            // per spawn with the user's config path baked in when it exists.
            let mut config = String::from("# mutiny nu config wrapper\n");
            if let Some(user) = nu_user_config("XDG_CONFIG_HOME", "config.nu") {
                config.push_str(&format!("source {}\n", user.display()));
            }
            config.push_str(&format!("source {}\n", dir.join("nu.nu").display()));
            write_atomic(&dir.join("nu-config.nu"), &config)?;

            let mut env_config = String::from("# mutiny nu env wrapper\n");
            if let Some(user) = nu_user_config("XDG_CONFIG_HOME", "env.nu") {
                env_config.push_str(&format!("source {}\n", user.display()));
            }
            write_atomic(&dir.join("nu-env.nu"), &env_config)?;

            inj.args.push(String::from("--config"));
            inj.args
                .push(dir.join("nu-config.nu").display().to_string());
            inj.args.push(String::from("--env-config"));
            inj.args.push(dir.join("nu-env.nu").display().to_string());
        }
        ShellKind::Other => return Ok(None),
    }

    inj.args.extend(existing_args.iter().cloned());
    Ok(Some(inj))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_basename_and_login_prefix() {
        assert_eq!(classify("/bin/bash"), ShellKind::Bash);
        assert_eq!(classify("-bash"), ShellKind::Bash);
        assert_eq!(classify("/usr/bin/zsh"), ShellKind::Zsh);
        assert_eq!(classify("fish"), ShellKind::Fish);
        assert_eq!(classify("/usr/bin/nu"), ShellKind::Nushell);
        assert_eq!(classify("nushell"), ShellKind::Nushell);
        assert_eq!(classify("/bin/sh"), ShellKind::Other);
        assert_eq!(classify("/bin/dash"), ShellKind::Other);
        assert_eq!(classify("/usr/bin/python3"), ShellKind::Other);
    }

    #[test]
    fn assets_install_idempotently_and_wrappers_are_present() {
        let dir = std::env::temp_dir().join(format!("mutiny-si-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        install_assets(&dir).expect("install");
        install_assets(&dir).expect("reinstall");
        for name in [
            "bash.sh",
            "bash_rc.sh",
            "zsh.sh",
            "fish.fish",
            "nu.nu",
            "zdot/.zshenv",
            "zdot/.zprofile",
            "zdot/.zshrc",
        ] {
            assert!(dir.join(name).is_file(), "missing {name}");
        }
        // Every snippet emits the markers the engine parses.
        for body in [BASH, ZSH, FISH, NU] {
            assert!(body.contains("133;A"), "snippet missing prompt marker");
            assert!(body.contains("133;C"), "snippet missing command marker");
            assert!(body.contains("133;D"), "snippet missing finish marker");
            assert!(body.contains("7;file://"), "snippet missing cwd marker");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn injection_preserves_caller_args_and_defers_to_explicit_config() {
        let dir = Path::new("/run/mutiny-test");
        let bash = prepare(ShellKind::Bash, dir, &[String::from("-i")])
            .expect("prepare")
            .expect("bash injection");
        assert_eq!(bash.args[0], "--rcfile");
        assert!(bash.args[1].ends_with("bash_rc.sh"));
        assert_eq!(bash.args.last().unwrap(), "-i");

        assert!(
            prepare(ShellKind::Bash, dir, &[String::from("--rcfile")])
                .unwrap()
                .is_none(),
            "caller's own rcfile must win"
        );
        assert!(prepare(ShellKind::Other, dir, &[]).unwrap().is_none());

        let fish = prepare(ShellKind::Fish, dir, &[]).unwrap().unwrap();
        assert_eq!(fish.args[0], "-C");
        assert!(fish.args[1].contains("fish.fish"));

        let zsh = prepare(ShellKind::Zsh, dir, &[]).unwrap().unwrap();
        assert!(
            zsh.env
                .iter()
                .any(|(k, v)| k == "ZDOTDIR" && v.ends_with("zdot"))
        );
    }
}
