//! TOML config at `$XDG_CONFIG_HOME/mutiny/config.toml`, with live reload.
//!
//! Precedence is CLI flag, then config file, then built-in default. Overrides
//! are re-applied on every reload, so editing the file never discards a flag
//! the operator launched with.
//!
//! Unknown keys are rejected rather than ignored, so a typo is reported instead
//! of silently doing nothing. A file that fails to parse leaves the previous
//! config in place and warns; it never takes the terminal down.

use std::path::{Path, PathBuf};
use std::time::Duration;

use alacritty_terminal::vte::ansi::Rgb;
use serde::{Deserialize, Serialize};

/// How often the tick stats the config file. A stat is cheap, but not every 4 ms.
pub const RELOAD_CHECK_INTERVAL: Duration = Duration::from_millis(250);

pub const DEFAULT_PALETTE: [&str; 16] = [
    "#1e222a", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
    "#5c6370", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#cba6f7", "#94e2d5", "#ffffff",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontConfig {
    pub family: String,
    pub size: u32,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: String::from("Monospace"),
            size: 11,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ColorConfig {
    pub background: String,
    pub foreground: String,
    pub cursor: String,
    pub selection: String,
    pub accent: String,
    /// Exactly 16 entries, ANSI 0-15. A wrong length falls back to the default
    /// palette rather than rendering with a partial one.
    pub palette: Vec<String>,
}

impl Default for ColorConfig {
    fn default() -> Self {
        Self {
            background: String::from("#16181d"),
            foreground: String::from("#e6e9ef"),
            cursor: String::from("#89b4fa"),
            selection: String::from("#3b5278"),
            accent: String::from("#5c9ef5"),
            palette: DEFAULT_PALETTE.iter().map(|s| s.to_string()).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GeometryConfig {
    /// Grid a new pane spawns at before its first size allocation.
    pub initial_cols: usize,
    pub initial_rows: usize,
    pub window_width: i32,
    pub window_height: i32,
    pub padding_px: f32,
    /// Height cap for the wrapping tab strip. Past this it scrolls vertically,
    /// so many tabs cannot squeeze the terminal out of the window.
    pub tab_strip_max_height_px: i32,
}

impl Default for GeometryConfig {
    fn default() -> Self {
        Self {
            initial_cols: 100,
            initial_rows: 30,
            window_width: 980,
            window_height: 640,
            padding_px: 6.0,
            tab_strip_max_height_px: 96,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalConfig {
    pub scrollback_lines: usize,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            scrollback_lines: 10_000,
        }
    }
}

/// Session picker: which multiplexers to run, which hosts to scan, and the
/// policy for attaching. Tunables live here rather than as constants beside the
/// code, so a test can drive everything through a temp TOML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionsConfig {
    /// Remote hosts to scan. Empty means fall back to literal `Host` aliases in
    /// ~/.ssh/config.
    pub hosts: Vec<String>,
    pub tmux_program: String,
    pub zellij_program: String,
    pub ssh_program: String,
    /// tmux `-S` socket. Set this to a private path to keep experiments away
    /// from your live sessions.
    pub tmux_socket: Option<String>,
    pub connect_timeout_s: u32,
    /// Wall-clock budget for a whole discovery pass before it reports partial.
    pub discovery_timeout_ms: u64,
    /// TERM sent to remote hosts. mutiny advertises `alacritty`, which they will
    /// not have terminfo for.
    pub remote_term: String,
    /// Attach-or-create (`tmux new-session -A`, `zellij attach --create`).
    pub create_if_missing: bool,
    /// tmux `-f ignore-size`: do not let this client resize other viewers.
    pub ignore_size: bool,
    /// Show zellij's resurrectable EXITED sessions, badged and sorted last.
    pub show_exited: bool,
    /// Re-list sessions every N seconds so the Sessions submenu stays current.
    /// 0 disables the periodic refresh; startup and picker opens still scan.
    pub refresh_interval_s: u32,
    /// Which multiplexer New Tab uses: "tmux", "zellij", or "none" for a plain
    /// shell. The New Session dialog can override it per session. This changes
    /// what Ctrl+Shift+T does; "none" restores the plain-shell behaviour.
    pub preferred: String,
    /// Optional conductor/root session name (default "meta") under which
    /// sibling local multiplexer sessions nest in `session-tree` and the
    /// Session Picker; set to "" for a flat local session list.
    pub root_session: String,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            hosts: Vec::new(),
            tmux_program: String::from("tmux"),
            zellij_program: String::from("zellij"),
            ssh_program: String::from("ssh"),
            tmux_socket: None,
            connect_timeout_s: 5,
            discovery_timeout_ms: 8_000,
            remote_term: String::from("xterm-256color"),
            create_if_missing: true,
            ignore_size: false,
            show_exited: true,
            refresh_interval_s: 15,
            preferred: String::from("tmux"),
            root_session: String::from("meta"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PeerConfig {
    /// Relay pending consent prompts to a connected, signer-enrolled outbound
    /// Haven limb when no Mutiny window has compositor focus.
    pub relay_unfocused_consent: bool,
    /// Grace window in milliseconds before an unfocused prompt relays to Haven.
    pub unfocused_grace_ms: u64,
    /// Optional default Haven SSH profile id or label for `handoff-to-peer` and
    /// workstation file streaming in `present-to-peer`.
    pub default_haven_ssh_profile: Option<String>,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            relay_unfocused_consent: true,
            unfocused_grace_ms: 1_500,
            default_haven_ssh_profile: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerRelayMode {
    #[default]
    Unfocused,
    Always,
    Off,
}

impl PeerRelayMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unfocused => "unfocused",
            Self::Always => "always",
            Self::Off => "off",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    /// Seconds before a finished unfocused OSC 133;D command raises an
    /// attention badge and notification (0 disables).
    pub long_command_secs: u64,
    /// When local notifications relay to connected Haven limbs:
    /// "unfocused", "always", or "off".
    pub peer_relay_mode: String,
    /// Case-insensitive package-name substrings opting into Android shade
    /// polling via Haven's `list_notifications`. Empty means Haven-own events
    /// only (zero `list_notifications` calls).
    pub inbound_shade_allowlist: Vec<String>,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            long_command_secs: 20,
            peer_relay_mode: String::from("unfocused"),
            inbound_shade_allowlist: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font: FontConfig,
    pub colors: ColorConfig,
    pub geometry: GeometryConfig,
    pub terminal: TerminalConfig,
    pub sessions: SessionsConfig,
    pub peer: PeerConfig,
    pub notifications: NotificationsConfig,
}

/// CLI flags that shadow config keys. Applied after the file on every load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub font_family: Option<String>,
    pub font_size: Option<u32>,
    pub initial_cols: Option<usize>,
    pub initial_rows: Option<usize>,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Config with colors parsed and numbers clamped, ready for the renderer and
/// the spawner to read without doing work per frame.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedConfig {
    pub font_family: String,
    pub font_size: u32,
    pub font_description: String,
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor: Rgb,
    pub selection: Rgb,
    pub accent: Rgb,
    pub palette: [Rgb; 16],
    pub initial_cols: usize,
    pub initial_rows: usize,
    pub window_width: i32,
    pub window_height: i32,
    pub padding_px: f32,
    pub tab_strip_max_height_px: i32,
    pub scrollback_lines: usize,
    pub sessions: crate::mux::MuxOptions,
    pub session_hosts: Vec<String>,
    pub show_exited_sessions: bool,
    pub session_refresh_interval_s: u32,
    /// `None` means New Tab opens a plain shell.
    pub preferred_mux: Option<crate::mux::MuxKind>,
    pub root_session: Option<String>,
    pub discovery_timeout_ms: u64,
    pub relay_unfocused_consent: bool,
    pub unfocused_grace_ms: u64,
    pub default_haven_ssh_profile: Option<String>,
    pub long_command_secs: u64,
    pub peer_relay_mode: PeerRelayMode,
    pub inbound_shade_allowlist: Vec<String>,
}

impl Default for ResolvedConfig {
    fn default() -> Self {
        resolve(&Config::default(), &Overrides::default())
    }
}

/// Serialises a config back to TOML for the Settings dialog to write. The
/// struct is the single source of truth for the key set, so a new field is
/// editable as soon as it exists here.
pub fn to_toml(config: &Config) -> Result<String, String> {
    toml::to_string_pretty(config).map_err(|e| e.to_string())
}

/// Reads the config file if present, else the defaults. The Settings dialog
/// starts from this so it shows what is actually in effect.
pub fn load_or_default(path: &Path) -> Config {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<Config>(&text).ok())
        .unwrap_or_default()
}

pub fn config_path() -> PathBuf {
    config_path_from(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Split out so tests can exercise path resolution without touching the env.
pub fn config_path_from(xdg_config_home: Option<&str>, home: Option<&str>) -> PathBuf {
    let base = xdg_config_home
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("mutiny").join("config.toml")
}

/// Reads and parses the config. A missing file is not an error: a fresh install
/// runs on defaults.
pub fn load_file(path: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

pub fn parse_hex(value: &str) -> Option<Rgb> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(Rgb {
        r: byte(0)?,
        g: byte(2)?,
        b: byte(4)?,
    })
}

fn color_or_default(value: &str, fallback: &str, key: &str) -> Rgb {
    match parse_hex(value) {
        Some(rgb) => rgb,
        None => {
            eprintln!("warning: config {key} = {value:?} is not #rrggbb; using {fallback}");
            parse_hex(fallback).expect("built-in default colors are valid")
        }
    }
}

/// Applies overrides on top of the file, then parses and clamps. Precedence:
/// CLI flag, then config file, then built-in default.
/// Parses the preferred-multiplexer setting. An unrecognised value falls back
/// to a plain shell with a warning, rather than silently picking a multiplexer
/// the user did not ask for.
fn parse_preferred(text: &str) -> Option<crate::mux::MuxKind> {
    match text.trim().to_ascii_lowercase().as_str() {
        "tmux" => Some(crate::mux::MuxKind::Tmux),
        "zellij" => Some(crate::mux::MuxKind::Zellij),
        "none" | "" => None,
        other => {
            eprintln!(
                "warning: config sessions.preferred {other:?} is not tmux, zellij, or none; \
                 New Tab will open a plain shell"
            );
            None
        }
    }
}

fn parse_peer_relay_mode(text: &str) -> PeerRelayMode {
    match text.trim().to_ascii_lowercase().as_str() {
        "unfocused" | "" => PeerRelayMode::Unfocused,
        "always" => PeerRelayMode::Always,
        "off" => PeerRelayMode::Off,
        other => {
            eprintln!(
                "warning: config notifications.peer_relay_mode {other:?} is not unfocused, always, or off; \
                 defaulting to unfocused"
            );
            PeerRelayMode::Unfocused
        }
    }
}

pub fn resolve(config: &Config, overrides: &Overrides) -> ResolvedConfig {
    let defaults = Config::default();
    let family = overrides
        .font_family
        .clone()
        .unwrap_or_else(|| config.font.family.clone());
    let size = overrides.font_size.unwrap_or(config.font.size).clamp(6, 72);

    let mut palette = [Rgb { r: 0, g: 0, b: 0 }; 16];
    if config.colors.palette.len() == 16 {
        for (slot, entry) in palette.iter_mut().zip(&config.colors.palette) {
            *slot = color_or_default(entry, DEFAULT_PALETTE[0], "colors.palette entry");
        }
    } else {
        eprintln!(
            "warning: config colors.palette has {} entries, expected 16; using the default palette",
            config.colors.palette.len()
        );
        for (slot, entry) in palette.iter_mut().zip(DEFAULT_PALETTE) {
            *slot = parse_hex(entry).expect("default palette is valid");
        }
    }

    let cols = overrides
        .initial_cols
        .unwrap_or(config.geometry.initial_cols)
        .clamp(20, 500);
    let rows = overrides
        .initial_rows
        .unwrap_or(config.geometry.initial_rows)
        .clamp(5, 200);

    ResolvedConfig {
        font_description: format!("{family} {size}"),
        font_family: family,
        font_size: size,
        background: color_or_default(
            &config.colors.background,
            &defaults.colors.background,
            "colors.background",
        ),
        foreground: color_or_default(
            &config.colors.foreground,
            &defaults.colors.foreground,
            "colors.foreground",
        ),
        cursor: color_or_default(
            &config.colors.cursor,
            &defaults.colors.cursor,
            "colors.cursor",
        ),
        selection: color_or_default(
            &config.colors.selection,
            &defaults.colors.selection,
            "colors.selection",
        ),
        accent: color_or_default(
            &config.colors.accent,
            &defaults.colors.accent,
            "colors.accent",
        ),
        palette,
        initial_cols: cols,
        initial_rows: rows,
        window_width: config.geometry.window_width.clamp(320, 7680),
        window_height: config.geometry.window_height.clamp(200, 4320),
        padding_px: config.geometry.padding_px.clamp(0.0, 64.0),
        tab_strip_max_height_px: config.geometry.tab_strip_max_height_px.clamp(32, 600),
        scrollback_lines: config.terminal.scrollback_lines.min(1_000_000),
        sessions: crate::mux::MuxOptions {
            tmux_program: config.sessions.tmux_program.clone(),
            zellij_program: config.sessions.zellij_program.clone(),
            ssh_program: config.sessions.ssh_program.clone(),
            tmux_socket: config.sessions.tmux_socket.clone(),
            connect_timeout_s: config.sessions.connect_timeout_s.clamp(1, 60),
            remote_term: config.sessions.remote_term.clone(),
            create_if_missing: config.sessions.create_if_missing,
            ignore_size: config.sessions.ignore_size,
        },
        session_hosts: config
            .sessions
            .hosts
            .iter()
            .filter(|h| match crate::mux::validate_host(h) {
                Ok(()) => true,
                Err(err) => {
                    eprintln!("warning: config sessions.hosts entry skipped: {err}");
                    false
                }
            })
            .take(crate::mux::MAX_HOSTS_PER_REQUEST)
            .cloned()
            .collect(),
        show_exited_sessions: config.sessions.show_exited,
        session_refresh_interval_s: config.sessions.refresh_interval_s.min(3600),
        preferred_mux: parse_preferred(&config.sessions.preferred),
        root_session: {
            let trimmed = config.sessions.root_session.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        },
        discovery_timeout_ms: config.sessions.discovery_timeout_ms.clamp(200, 60_000),
        relay_unfocused_consent: config.peer.relay_unfocused_consent,
        unfocused_grace_ms: config.peer.unfocused_grace_ms.min(60_000),
        default_haven_ssh_profile: config
            .peer
            .default_haven_ssh_profile
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        long_command_secs: config.notifications.long_command_secs.min(86_400),
        peer_relay_mode: parse_peer_relay_mode(&config.notifications.peer_relay_mode),
        inbound_shade_allowlist: {
            let mut list: Vec<String> = config
                .notifications
                .inbound_shade_allowlist
                .iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .take(32)
                .collect();
            list.dedup();
            list
        },
    }
}

/// Modification time used to detect an edit. None when the file is absent.
pub fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mutiny-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = temp_dir("missing");
        let path = dir.join("nope.toml");
        let cfg = load_file(&path).expect("missing file is not an error");
        assert_eq!(cfg, Config::default());
        let resolved = resolve(&cfg, &Overrides::default());
        assert_eq!(resolved.font_description, "Monospace 11");
        assert_eq!(resolved.initial_cols, 100);
        assert_eq!(
            resolved.background,
            Rgb {
                r: 0x16,
                g: 0x18,
                b: 0x1d
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_config_merges_with_defaults() {
        let dir = temp_dir("partial");
        let path = write(&dir, "[font]\nsize = 14\n");
        let cfg = load_file(&path).expect("parse");
        assert_eq!(cfg.font.size, 14);
        assert_eq!(cfg.font.family, "Monospace");
        assert_eq!(cfg.colors, ColorConfig::default());
        assert_eq!(cfg.geometry, GeometryConfig::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_config_drives_font_palette_and_geometry() {
        let dir = temp_dir("full");
        let path = write(
            &dir,
            r##"
[font]
family = "JetBrains Mono"
size = 13

[colors]
background = "#101014"
foreground = "#f0f0f0"
cursor = "#ff0000"
selection = "#203040"
accent = "#00ff00"
palette = [
  "#000000", "#800000", "#008000", "#808000",
  "#000080", "#800080", "#008080", "#c0c0c0",
  "#808080", "#ff0000", "#00ff00", "#ffff00",
  "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
]

[geometry]
initial_cols = 120
initial_rows = 40
window_width = 1200
window_height = 800
padding_px = 10.0

[terminal]
scrollback_lines = 5000
"##,
        );
        let resolved = resolve(&load_file(&path).expect("parse"), &Overrides::default());
        assert_eq!(resolved.font_description, "JetBrains Mono 13");
        assert_eq!(
            resolved.background,
            Rgb {
                r: 0x10,
                g: 0x10,
                b: 0x14
            }
        );
        assert_eq!(
            resolved.cursor,
            Rgb {
                r: 0xff,
                g: 0,
                b: 0
            }
        );
        assert_eq!(
            resolved.palette[1],
            Rgb {
                r: 0x80,
                g: 0,
                b: 0
            }
        );
        assert_eq!(
            resolved.palette[15],
            Rgb {
                r: 0xff,
                g: 0xff,
                b: 0xff
            }
        );
        assert_eq!(resolved.initial_cols, 120);
        assert_eq!(resolved.initial_rows, 40);
        assert_eq!(resolved.window_width, 1200);
        assert_eq!(resolved.padding_px, 10.0);
        assert_eq!(resolved.scrollback_lines, 5000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_overrides_beat_the_file_and_the_file_beats_defaults() {
        let dir = temp_dir("precedence");
        let path = write(
            &dir,
            "[font]\nfamily = \"FromFile\"\nsize = 12\n[geometry]\ninitial_cols = 90\n",
        );
        let cfg = load_file(&path).expect("parse");

        // File beats default.
        let no_flags = resolve(&cfg, &Overrides::default());
        assert_eq!(no_flags.font_description, "FromFile 12");
        assert_eq!(no_flags.initial_cols, 90);

        // Flag beats file.
        let flags = Overrides {
            font_family: Some(String::from("FromFlag")),
            font_size: Some(20),
            initial_cols: Some(132),
            initial_rows: None,
        };
        let with_flags = resolve(&cfg, &flags);
        assert_eq!(with_flags.font_description, "FromFlag 20");
        assert_eq!(with_flags.initial_cols, 132);
        // A key with no flag still comes from the file.
        assert_eq!(with_flags.font_family, "FromFlag");

        // Flag beats default when the file says nothing.
        let empty = resolve(&Config::default(), &flags);
        assert_eq!(empty.font_description, "FromFlag 20");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_toml_is_an_error_and_unknown_keys_are_rejected() {
        let dir = temp_dir("invalid");
        let bad = write(&dir, "[font\nsize = ");
        assert!(load_file(&bad).is_err(), "malformed TOML must be reported");

        let typo = write(&dir, "[font]\nsizee = 12\n");
        let err = load_file(&typo).expect_err("unknown key must be rejected");
        assert!(
            err.contains("sizee"),
            "error should name the bad key: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_color_and_wrong_palette_length_fall_back_without_failing() {
        let dir = temp_dir("badcolor");
        let path = write(
            &dir,
            "[colors]\nbackground = \"not-a-color\"\npalette = [\"#000000\"]\n",
        );
        let cfg = load_file(&path).expect("parse still succeeds");
        let resolved = resolve(&cfg, &Overrides::default());
        assert_eq!(
            resolved.background,
            parse_hex(&ColorConfig::default().background).unwrap(),
            "invalid color falls back to the default"
        );
        assert_eq!(
            resolved.palette,
            resolve(&Config::default(), &Overrides::default()).palette,
            "wrong palette length falls back to the full default palette"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_range_numbers_are_clamped() {
        let dir = temp_dir("clamp");
        let path = write(
            &dir,
            "[font]\nsize = 900\n[geometry]\ninitial_cols = 1\ninitial_rows = 9999\npadding_px = -5.0\n",
        );
        let resolved = resolve(&load_file(&path).unwrap(), &Overrides::default());
        assert_eq!(resolved.font_size, 72);
        assert_eq!(resolved.initial_cols, 20);
        assert_eq!(resolved.initial_rows, 200);
        assert_eq!(resolved.padding_px, 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn to_toml_round_trips_every_default_key() {
        let dir = std::env::temp_dir().join(format!("mutiny-cfg-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let original = Config::default();
        let text = to_toml(&original).expect("serialise");
        std::fs::write(&path, &text).unwrap();
        let back = load_or_default(&path);
        assert_eq!(back, original, "every key must survive a write/read cycle");
        // A missing file yields defaults rather than an error.
        assert_eq!(load_or_default(&dir.join("absent.toml")), Config::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_resolution_prefers_xdg_then_home() {
        assert_eq!(
            config_path_from(Some("/xdg"), Some("/home/user")),
            PathBuf::from("/xdg/mutiny/config.toml")
        );
        assert_eq!(
            config_path_from(None, Some("/home/user")),
            PathBuf::from("/home/user/.config/mutiny/config.toml")
        );
        assert_eq!(
            config_path_from(Some(""), Some("/home/user")),
            PathBuf::from("/home/user/.config/mutiny/config.toml")
        );
    }

    #[test]
    fn hex_parsing_accepts_both_forms_and_rejects_junk() {
        assert_eq!(
            parse_hex("#ff8000"),
            Some(Rgb {
                r: 255,
                g: 128,
                b: 0
            })
        );
        assert_eq!(
            parse_hex("ff8000"),
            Some(Rgb {
                r: 255,
                g: 128,
                b: 0
            })
        );
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("#gggggg"), None);
        assert_eq!(parse_hex(""), None);
    }

    #[test]
    fn mtime_changes_when_the_file_is_edited() {
        let dir = temp_dir("mtime");
        let path = write(&dir, "[font]\nsize = 11\n");
        let first = file_mtime(&path).expect("mtime");
        assert!(file_mtime(&dir.join("absent.toml")).is_none());
        // Force a distinct mtime rather than relying on filesystem granularity.
        let later = first + Duration::from_secs(2);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_ne!(file_mtime(&path), Some(first));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
