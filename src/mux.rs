//! Multiplexer session discovery: parsing, argv construction, and quoting.
//!
//! Pure data, like `graphics.rs`. No GTK and no subprocess execution here, so
//! every rule below is unit-testable without tmux, zellij, or ssh installed.
//! The controller owns the child processes and the tick budget.
//!
//! Mutiny does not reimplement multiplexing (VISION.md, scope boundaries). This
//! module only builds the commands that hand a pane over to an existing tmux or
//! zellij session, and reads back what those programs report.
//!
//! Three hazards shaped this code, each verified against the real programs:
//!
//! 1. A tmux session name may contain `|`, spaces, quotes, and `$`. The listing
//!    format therefore separates fields with a literal TAB, which a stored name
//!    cannot contain because tmux escapes control characters at creation.
//! 2. tmux re-parses a trailing `;` in its own argv even with no shell involved,
//!    so a name ending in `;` needs one backslash before that final character.
//!    Interior semicolons must NOT be escaped: tmux preserves the backslash.
//! 3. ssh joins its command arguments into a string the remote login shell
//!    parses. Every element of a remote command is POSIX-quoted, including the
//!    format string, and the host is preceded by `--` so a leading-dash host
//!    cannot be read as an ssh option.

use serde::{Deserialize, Serialize};

/// Bounds, in the same spirit as the rest of the crate: nothing unbounded.
pub const MAX_SESSIONS_PER_HOST: usize = 512;
pub const MAX_DISCOVERY_BYTES: usize = 256 * 1024;
pub const MAX_CONCURRENT_DISCOVERIES: usize = 4;
pub const MAX_HOSTS_PER_REQUEST: usize = 16;
/// Bytes drained from discovery pipes per tick, shared across active jobs the
/// way `SEARCH_LINES_PER_TICK` is shared across searches.
pub const DISCOVERY_BYTES_PER_TICK: usize = 64 * 1024;

/// TAB-separated on purpose. `|` is legal in a session name and would corrupt
/// the parse; a raw TAB cannot survive tmux's name cleaning.
pub const TMUX_LIST_FORMAT: &str =
    "#{session_name}\t#{session_windows}\t#{session_attached}\t#{session_created}\t#{session_path}";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MuxKind {
    Tmux,
    Zellij,
}

impl MuxKind {
    pub fn label(self) -> &'static str {
        match self {
            MuxKind::Tmux => "tmux",
            MuxKind::Zellij => "zellij",
        }
    }
}

/// Where to open a session. Reuses `spawn_pane`'s vocabulary so an agent learns
/// one set of words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionMode {
    Window,
    Tab,
    SplitRight,
    SplitDown,
}

impl SessionMode {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "window" => Some(Self::Window),
            "tab" => Some(Self::Tab),
            "split-right" => Some(Self::SplitRight),
            "split-down" => Some(Self::SplitDown),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MuxSession {
    /// `None` means this machine.
    pub host: Option<String>,
    pub kind: MuxKind,
    pub name: String,
    /// zellij does not report a window count in its listing, so it stays 0.
    pub windows: u32,
    /// A count, not a boolean: tmux reports how many clients are attached.
    /// zellij does not report it, so it stays 0.
    pub attached: u32,
    pub created: u64,
    /// tmux's `session_path`. Never used as a local cwd for a remote session.
    pub path: Option<String>,
    /// zellij marks dead sessions resurrectable. tmux sessions are always live.
    pub exited: bool,
}

impl MuxSession {
    /// Stable identity for preserving picker selection across refreshes.
    pub fn identity(&self) -> (Option<String>, MuxKind, String) {
        (self.host.clone(), self.kind, self.name.clone())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostState {
    Ok,
    Scanning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MuxHostStatus {
    pub host: Option<String>,
    pub kind: MuxKind,
    pub state: HostState,
    /// Sanitized, single-line. Distinguishes "not installed", "no server", and
    /// "transport/auth failed", because the user's remedy differs for each.
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MuxInventory {
    pub sessions: Vec<MuxSession>,
    pub hosts: Vec<MuxHostStatus>,
    /// A deadline passed before every job answered.
    pub partial: bool,
    pub scanned_at_ms: u64,
}

impl MuxInventory {
    /// Live local sessions first, then live remote, then exited ones. A wall of
    /// nine-month-old resurrectable sessions is not a useful default view.
    pub fn sorted_for_display(&self) -> Vec<&MuxSession> {
        let mut out: Vec<&MuxSession> = self.sessions.iter().collect();
        out.sort_by_key(|s| (s.exited, s.host.is_some(), s.host.clone(), s.name.clone()));
        out
    }
}

/// Programs and policy. Mirrors the `[sessions]` config table so nothing here
/// is a constant beside the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxOptions {
    pub tmux_program: String,
    pub zellij_program: String,
    pub ssh_program: String,
    /// `-S` socket. Tests use a private one so they never touch live sessions.
    pub tmux_socket: Option<String>,
    pub connect_timeout_s: u32,
    /// mutiny advertises TERM=alacritty, which remote hosts will not have.
    pub remote_term: String,
    pub create_if_missing: bool,
    /// tmux `-f ignore-size`: do not let this client resize other viewers.
    pub ignore_size: bool,
}

impl Default for MuxOptions {
    fn default() -> Self {
        Self {
            tmux_program: String::from("tmux"),
            zellij_program: String::from("zellij"),
            ssh_program: String::from("ssh"),
            tmux_socket: None,
            connect_timeout_s: 5,
            remote_term: String::from("xterm-256color"),
            create_if_missing: true,
            ignore_size: false,
        }
    }
}

/// Replaces control characters and truncates, so a hostile session name or a
/// remote stderr line cannot put escape sequences into the audit trail or the
/// picker.
pub fn sanitize_text(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if out.chars().count() >= max_chars {
            out.push('…');
            break;
        }
        if ch.is_control() {
            out.push('?');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Parses `tmux list-sessions -F TMUX_LIST_FORMAT`. Malformed lines are skipped
/// rather than turned into sessions, so an error message on stdout cannot
/// become a phantom entry.
pub fn parse_tmux(out: &str, host: Option<&str>) -> Vec<MuxSession> {
    let mut sessions = Vec::new();
    for line in out.lines() {
        if sessions.len() >= MAX_SESSIONS_PER_HOST {
            break;
        }
        let fields: Vec<&str> = line.splitn(5, '\t').collect();
        if fields.len() < 4 {
            continue;
        }
        let name = fields[0];
        if name.is_empty() {
            continue;
        }
        let (Ok(windows), Ok(attached)) = (fields[1].parse::<u32>(), fields[2].parse::<u32>())
        else {
            continue;
        };
        let created = fields[3].parse::<u64>().unwrap_or(0);
        let path = fields
            .get(4)
            .filter(|p| !p.is_empty())
            .map(|p| (*p).to_owned());
        sessions.push(MuxSession {
            host: host.map(str::to_owned),
            kind: MuxKind::Tmux,
            name: name.to_owned(),
            windows,
            attached,
            created,
            path,
            exited: false,
        });
    }
    sessions
}

/// Parses the LONG `zellij list-sessions --no-formatting` form. The `--short`
/// form is deliberately not used: it is byte-identical to `--short
/// --no-formatting` and hides the EXITED marker entirely.
pub fn parse_zellij(out: &str, host: Option<&str>) -> Vec<MuxSession> {
    let mut sessions = Vec::new();
    for line in out.lines() {
        if sessions.len() >= MAX_SESSIONS_PER_HOST {
            break;
        }
        let line = line.trim();
        let Some((name, rest)) = line.split_once(" [Created") else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        sessions.push(MuxSession {
            host: host.map(str::to_owned),
            kind: MuxKind::Zellij,
            name: name.to_owned(),
            windows: 0,
            attached: 0,
            created: 0,
            path: None,
            exited: rest.contains("EXITED"),
        });
    }
    sessions
}

/// Layer 1: tmux's own argv re-parse. One backslash before a FINAL `;` only.
/// Escaping interior semicolons would corrupt the name, because tmux keeps the
/// backslash there.
pub fn tmux_escape_name(name: &str) -> String {
    match name.strip_suffix(';') {
        Some(head) => format!("{head}\\;"),
        None => name.to_owned(),
    }
}

/// Layer 2: the remote login shell. Wrap in single quotes and splice any
/// embedded quote as `'\''`.
pub fn shell_quote_posix(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A host arrives from IPC, so it is untrusted. A leading dash would be read as
/// an ssh option (`-oProxyCommand=…` is local code execution), and whitespace or
/// glob characters have no business in a destination.
pub fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err(String::from("host must not be empty"));
    }
    if host.starts_with('-') {
        return Err(format!(
            "host {host:?} starts with '-' and would be read as an ssh option"
        ));
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ".-_@:[]%".contains(c))
    {
        return Err(format!(
            "host {host:?} contains characters not allowed in an ssh destination"
        ));
    }
    Ok(())
}

/// Literal `Host` aliases from an ssh_config body. Wildcards, negations, and
/// `Match` criteria are skipped: they are patterns, not destinations.
pub fn ssh_config_hosts(text: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let Some(keyword) = tokens.next() else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        for token in tokens {
            if token.contains(['*', '?', '!']) || token.is_empty() {
                continue;
            }
            if !hosts.iter().any(|h| h == token) {
                hosts.push(token.to_owned());
            }
        }
    }
    hosts
}

fn tmux_socket_args(opts: &MuxOptions) -> Vec<String> {
    match &opts.tmux_socket {
        Some(sock) => vec![String::from("-S"), sock.clone()],
        None => Vec::new(),
    }
}

/// Inner listing argv, before any remote wrapping.
pub fn discovery_argv(kind: MuxKind, opts: &MuxOptions) -> Vec<String> {
    match kind {
        MuxKind::Tmux => {
            let mut argv = tmux_socket_args(opts);
            argv.push(String::from("list-sessions"));
            argv.push(String::from("-F"));
            argv.push(String::from(TMUX_LIST_FORMAT));
            argv
        }
        MuxKind::Zellij => vec![
            String::from("list-sessions"),
            String::from("--no-formatting"),
        ],
    }
}

/// Inner attach argv. `new-session -A` attaches or creates and resolves the
/// name exactly, unlike `attach-session -t`, which matches fuzzily.
pub fn inner_attach_argv(kind: MuxKind, name: &str, opts: &MuxOptions) -> Vec<String> {
    match kind {
        MuxKind::Tmux => {
            let mut argv = tmux_socket_args(opts);
            argv.push(String::from("new-session"));
            argv.push(String::from("-A"));
            if opts.ignore_size {
                argv.push(String::from("-f"));
                argv.push(String::from("ignore-size"));
            }
            argv.push(String::from("-s"));
            argv.push(tmux_escape_name(name));
            argv
        }
        MuxKind::Zellij => {
            let mut argv = vec![String::from("attach")];
            if opts.create_if_missing {
                argv.push(String::from("--create"));
            }
            argv.push(name.to_owned());
            argv
        }
    }
}

pub fn local_program(kind: MuxKind, opts: &MuxOptions) -> String {
    match kind {
        MuxKind::Tmux => opts.tmux_program.clone(),
        MuxKind::Zellij => opts.zellij_program.clone(),
    }
}

/// Local attach: exec the multiplexer directly, no shell anywhere.
pub fn attach_argv(kind: MuxKind, name: &str, opts: &MuxOptions) -> (String, Vec<String>) {
    (
        local_program(kind, opts),
        inner_attach_argv(kind, name, opts),
    )
}

/// Builds the string the remote login shell will parse. The program name is
/// part of it: quoting only the arguments would leave a command with no verb.
fn remote_command(program: &str, inner: &[String], term: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(term) = term {
        parts.push(format!("TERM={}", shell_quote_posix(term)));
    }
    parts.push(shell_quote_posix(program));
    parts.extend(inner.iter().map(|a| shell_quote_posix(a)));
    parts.join(" ")
}

/// Remote discovery: never prompt, never allocate a tty, hard connect timeout.
pub fn remote_discovery_argv(
    host: &str,
    kind: MuxKind,
    opts: &MuxOptions,
) -> Result<(String, Vec<String>), String> {
    validate_host(host)?;
    let inner = discovery_argv(kind, opts);
    let argv = vec![
        String::from("-n"),
        String::from("-o"),
        String::from("BatchMode=yes"),
        String::from("-o"),
        format!("ConnectTimeout={}", opts.connect_timeout_s),
        String::from("--"),
        host.to_owned(),
        remote_command(&local_program(kind, opts), &inner, None),
    ];
    Ok((opts.ssh_program.clone(), argv))
}

/// Remote attach: needs a tty, and must allow the user to type a passphrase, so
/// no BatchMode. `-e none` stops a typed `~.` from killing the session. TERM is
/// set in the remote command because sshd may not accept our TERM.
pub fn remote_attach_argv(
    host: &str,
    kind: MuxKind,
    name: &str,
    opts: &MuxOptions,
) -> Result<(String, Vec<String>), String> {
    validate_host(host)?;
    let inner = inner_attach_argv(kind, name, opts);
    let argv = vec![
        String::from("-t"),
        String::from("-e"),
        String::from("none"),
        String::from("-o"),
        format!("ConnectTimeout={}", opts.connect_timeout_s),
        String::from("--"),
        host.to_owned(),
        remote_command(&local_program(kind, opts), &inner, Some(&opts.remote_term)),
    ];
    Ok((opts.ssh_program.clone(), argv))
}

/// Classifies a discovery failure so the picker can say something actionable.
pub fn classify_failure(program: &str, code: Option<i32>, stderr: &str) -> String {
    let first = stderr.lines().next().unwrap_or("").trim();
    let detail = sanitize_text(first, 160);
    if code == Some(255) {
        return format!("ssh transport or auth failed: {detail}");
    }
    if first.contains("error connecting to") || first.contains("no server running") {
        return format!("{program}: no server running (no sessions)");
    }
    if first.contains("No active sessions") || first.contains("no sessions") {
        return format!("{program}: no sessions");
    }
    if detail.is_empty() {
        format!("{program} exited with {code:?}")
    } else {
        format!("{program}: {detail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> MuxOptions {
        MuxOptions::default()
    }

    #[test]
    fn tmux_format_uses_a_real_tab_not_a_pipe() {
        assert!(
            TMUX_LIST_FORMAT.contains('\t'),
            "must be a literal TAB byte"
        );
        assert!(!TMUX_LIST_FORMAT.contains('|'), "a pipe is legal in a name");
    }

    #[test]
    fn parse_tmux_reads_counts_and_keeps_hostile_names_intact() {
        let out = "ardupilot\t1\t2\t1792967297\t/home/ian\n\
                   a|b c'd\t3\t0\t1793642158\t/tmp/x\n";
        let sessions = parse_tmux(out, None);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "ardupilot");
        assert_eq!(sessions[0].attached, 2, "attached is a COUNT, not a bool");
        assert_eq!(sessions[0].windows, 1);
        assert_eq!(sessions[0].created, 1792967297);
        assert_eq!(sessions[0].path.as_deref(), Some("/home/ian"));
        assert_eq!(sessions[1].name, "a|b c'd", "pipe and quote must survive");
        assert_eq!(sessions[1].attached, 0);
        assert!(!sessions[0].exited);
    }

    #[test]
    fn parse_tmux_skips_malformed_lines_and_keeps_the_path_remainder() {
        let out = "error connecting to /tmp/tmux-1000/default\n\
                   \n\
                   good\t2\t1\t5\t/path\twith\ttabs\n\
                   bad\tnotanumber\t1\t5\n";
        let sessions = parse_tmux(out, Some("meteor"));
        assert_eq!(sessions.len(), 1, "only the well-formed row survives");
        assert_eq!(sessions[0].name, "good");
        assert_eq!(sessions[0].host.as_deref(), Some("meteor"));
        assert_eq!(
            sessions[0].path.as_deref(),
            Some("/path\twith\ttabs"),
            "splitn keeps the remainder intact"
        );
    }

    #[test]
    fn parse_zellij_long_form_sees_exited_sessions() {
        let out = "CAR [Created 9months 13days ago] (EXITED - attach to resurrect)\n\
                   liveone [Created 2days 3h ago]\n\
                   No active sessions found.\n";
        let sessions = parse_zellij(out, None);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "CAR");
        assert!(sessions[0].exited, "the EXITED marker must be visible");
        assert_eq!(sessions[1].name, "liveone");
        assert!(!sessions[1].exited);
        assert_eq!(sessions[0].kind, MuxKind::Zellij);
    }

    #[test]
    fn zellij_listing_argv_never_uses_short() {
        let argv = discovery_argv(MuxKind::Zellij, &opts());
        assert!(argv.contains(&String::from("--no-formatting")));
        assert!(
            !argv.contains(&String::from("--short")),
            "--short hides the EXITED marker"
        );
    }

    #[test]
    fn shell_quote_posix_round_trips_through_a_real_shell() {
        for raw in [
            "a'b",
            "a;echo INJECTED",
            "$(echo x)",
            "`echo x`",
            "with space",
            "",
            "a|b",
            "\"quoted\"",
            "\\back",
        ] {
            let quoted = shell_quote_posix(raw);
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf %s {quoted}"))
                .output()
                .expect("sh");
            assert!(out.status.success(), "sh rejected {quoted}");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                raw,
                "quoting changed the value for {raw:?}"
            );
            assert!(
                out.stderr.is_empty(),
                "quoting leaked execution for {raw:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn tmux_escape_only_touches_a_final_semicolon() {
        assert_eq!(tmux_escape_name("plain"), "plain");
        assert_eq!(tmux_escape_name("probe;"), "probe\\;");
        assert_eq!(tmux_escape_name("x;;"), "x;\\;");
        assert_eq!(
            tmux_escape_name("x;y"),
            "x;y",
            "an interior semicolon must NOT be escaped: tmux keeps the backslash"
        );
        assert_eq!(tmux_escape_name("a|b c"), "a|b c");
    }

    #[test]
    fn host_validation_rejects_option_injection_and_globs() {
        for bad in [
            "",
            "-oProxyCommand=touch /tmp/pwn",
            "-v",
            "a b",
            "*",
            "!x",
            "host?",
            "a\tb",
            "a\nb",
        ] {
            assert!(validate_host(bad).is_err(), "{bad:?} must be refused");
        }
        for good in [
            "garage-ap",
            "meteor",
            "user@host",
            "192.168.0.11",
            "[fe80::1]%eth0",
        ] {
            assert!(validate_host(good).is_ok(), "{good:?} must be accepted");
        }
    }

    #[test]
    fn ssh_config_hosts_takes_literal_aliases_only() {
        let text = "# comment\n\
                    Host garage-ap\n\
                    \tHostName 192.168.0.3\n\
                    Host *.lan !bad one two\n\
                    Match host foo\n\
                    Host one\n";
        assert_eq!(ssh_config_hosts(text), vec!["garage-ap", "one", "two"]);
    }

    #[test]
    fn local_attach_shares_and_never_detaches() {
        let (prog, argv) = attach_argv(MuxKind::Tmux, "work", &opts());
        assert_eq!(prog, "tmux");
        assert!(argv.contains(&String::from("new-session")));
        assert!(argv.contains(&String::from("-A")), "attach-or-create");
        assert!(
            !argv.iter().any(|a| a == "-d" || a == "-D"),
            "must never detach other clients"
        );
        let pos = argv.iter().position(|a| a == "-s").unwrap();
        assert_eq!(argv[pos + 1], "work");
    }

    #[test]
    fn local_attach_escapes_a_hostile_name() {
        let (_prog, argv) = attach_argv(MuxKind::Tmux, "probe;", &opts());
        let pos = argv.iter().position(|a| a == "-s").unwrap();
        assert_eq!(argv[pos + 1], "probe\\;");
    }

    #[test]
    fn private_socket_is_forwarded_to_every_tmux_command() {
        let mut o = opts();
        o.tmux_socket = Some(String::from("/tmp/private.sock"));
        for argv in [
            discovery_argv(MuxKind::Tmux, &o),
            inner_attach_argv(MuxKind::Tmux, "x", &o),
        ] {
            assert_eq!(argv[0], "-S");
            assert_eq!(argv[1], "/tmp/private.sock");
        }
    }

    #[test]
    fn remote_discovery_never_prompts_and_guards_the_host() {
        let (prog, argv) = remote_discovery_argv("meteor", MuxKind::Tmux, &opts()).unwrap();
        assert_eq!(prog, "ssh");
        assert!(argv.contains(&String::from("BatchMode=yes")));
        assert!(argv.contains(&String::from("-n")));
        assert!(
            !argv.contains(&String::from("-t")),
            "discovery needs no tty"
        );
        let dash = argv.iter().position(|a| a == "--").expect("-- guard");
        assert_eq!(argv[dash + 1], "meteor", "host must follow --");
        assert!(argv.last().unwrap().contains("list-sessions"));
        assert!(
            argv.last().unwrap().contains("'tmux'"),
            "the remote command must name the program, not just its arguments"
        );
        assert!(
            argv.last().unwrap().contains("'#{session_name}"),
            "the format string must be quoted too, or the remote shell splits it"
        );
        assert!(remote_discovery_argv("-oProxyCommand=x", MuxKind::Tmux, &opts()).is_err());
    }

    #[test]
    fn remote_attach_allocates_a_tty_and_sets_a_portable_term() {
        let (prog, argv) = remote_attach_argv("meteor", MuxKind::Tmux, "work", &opts()).unwrap();
        assert_eq!(prog, "ssh");
        assert!(argv.contains(&String::from("-t")), "attach needs a tty");
        assert!(argv.contains(&String::from("none")), "-e none");
        assert!(
            !argv.contains(&String::from("BatchMode=yes")),
            "attach must let the user type a passphrase"
        );
        let cmd = argv.last().unwrap();
        assert!(cmd.starts_with("TERM='xterm-256color'"), "{cmd}");
        assert!(cmd.contains("'new-session'"));
    }

    #[test]
    fn remote_attach_quotes_a_hostile_name_into_one_argument() {
        let (_prog, argv) =
            remote_attach_argv("meteor", MuxKind::Tmux, "a;echo x", &opts()).unwrap();
        let cmd = argv.last().unwrap().clone();
        // Run the exact string through a real shell with a stub tmux that
        // prints its argv, and assert the name arrived as ONE argument.
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("tmux() {{ printf '%s\\n' \"$@\"; }}; {cmd}"))
            .output()
            .expect("sh");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.lines().any(|l| l == "a;echo x"),
            "name must survive as one argument, got:\n{text}"
        );
        assert!(
            !text.lines().any(|l| l == "x"),
            "the injected command must not have run:\n{text}"
        );
    }

    #[test]
    fn ignore_size_is_opt_in_and_forwarded() {
        let (_p, plain) = attach_argv(MuxKind::Tmux, "w", &opts());
        assert!(!plain.contains(&String::from("ignore-size")));
        let mut o = opts();
        o.ignore_size = true;
        let (_p, argv) = attach_argv(MuxKind::Tmux, "w", &o);
        assert!(argv.contains(&String::from("ignore-size")));
    }

    #[test]
    fn zellij_attach_can_create_and_does_not_tmux_escape() {
        let (_p, argv) = attach_argv(MuxKind::Zellij, "probe;", &opts());
        assert!(argv.contains(&String::from("--create")));
        assert!(
            argv.contains(&String::from("probe;")),
            "zellij has no semicolon re-parse, so no backslash"
        );
        let mut o = opts();
        o.create_if_missing = false;
        let (_p, argv) = attach_argv(MuxKind::Zellij, "x", &o);
        assert!(!argv.contains(&String::from("--create")));
    }

    #[test]
    fn display_order_puts_live_local_sessions_first() {
        let inv = MuxInventory {
            sessions: vec![
                MuxSession {
                    host: None,
                    kind: MuxKind::Zellij,
                    name: "dead".into(),
                    windows: 0,
                    attached: 0,
                    created: 0,
                    path: None,
                    exited: true,
                },
                MuxSession {
                    host: Some("meteor".into()),
                    kind: MuxKind::Tmux,
                    name: "remote".into(),
                    windows: 1,
                    attached: 0,
                    created: 0,
                    path: None,
                    exited: false,
                },
                MuxSession {
                    host: None,
                    kind: MuxKind::Tmux,
                    name: "local".into(),
                    windows: 1,
                    attached: 1,
                    created: 0,
                    path: None,
                    exited: false,
                },
            ],
            ..Default::default()
        };
        let names: Vec<&str> = inv
            .sorted_for_display()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["local", "remote", "dead"]);
    }

    #[test]
    fn failure_classes_are_distinguishable() {
        assert!(
            classify_failure("tmux", Some(1), "error connecting to /tmp/x (No such file)")
                .contains("no server running")
        );
        assert!(
            classify_failure("ssh", Some(255), "Host key verification failed.")
                .contains("transport or auth")
        );
        assert!(
            classify_failure("zellij", Some(0), "No active sessions found.")
                .contains("no sessions")
        );
        let nasty = classify_failure("tmux", Some(1), "bad\x1b[31mname\nsecond line");
        assert!(!nasty.contains('\x1b'), "escapes must be sanitized");
        assert!(!nasty.contains('\n'), "must be a single line");
    }

    #[test]
    fn sanitize_bounds_length_and_control_characters() {
        assert_eq!(sanitize_text("a\tb\nc", 40), "a?b?c");
        let long = sanitize_text(&"x".repeat(500), 10);
        assert_eq!(long.chars().count(), 11, "10 chars plus the ellipsis");
    }

    #[test]
    fn session_parsing_is_bounded() {
        let many = "s\t1\t0\t0\t/p\n".repeat(MAX_SESSIONS_PER_HOST + 50);
        assert_eq!(parse_tmux(&many, None).len(), MAX_SESSIONS_PER_HOST);
    }
}
