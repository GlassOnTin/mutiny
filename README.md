# mutiny

[![CI](https://github.com/GlassOnTin/mutiny/actions/workflows/ci.yml/badge.svg)](https://github.com/GlassOnTin/mutiny/actions/workflows/ci.yml)
[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue.svg)](LICENSE)

A Linux desktop terminal in pure Rust and pure `gtk4-rs`. Panes move between splits, tabs, and windows without restarting their shells, and an agent can read and drive the same panes a human is watching, through the same verbs.

Wayland-first with X11 support, built for KDE Plasma and GTK desktops. No libadwaita, no C-ABI terminal core, no Zig toolchain. Server-side decorations are left to KWin, so Breeze titlebars work as they do for any other Qt or GTK application.

## Status

Early, and honest about it. M0 and M0.5 are shipped, along with several M1 rungs. The engine, pane mobility, and agent surface are covered by 71 tests. Most run with no display at all; the IME and font-fallback test needs GTK and runs under Xvfb in CI. Rendering to a real window is still not exercised by the suite.

Working today:

- `alacritty_terminal` 0.26 engine with a PTY per pane, 10 000 lines of scrollback, and line-level search.
- OSC 133 command journal, OSC 7 working directory, OSC 9;4 progress, OSC 8 hyperlinks, OSC 52 clipboard.
- Shell integration bundled for bash, zsh, fish, and nushell, injected automatically at spawn. A fresh install gets prompt jumping, command-output capture, and exit-code reporting without editing any dotfile.
- Binary split trees and tabs. `detach-split` pops a pane into its own window and `move-pane` moves it into another tab, both without touching the running shell or its scrollback.
- `urgent-next` jumps across windows, tabs, and splits to the next pane needing attention (a bell, a failed command, or an agent notice).
- A ring of the last eight exited panes keeps the final screen and exit code, so a session that fails on startup is still diagnosable.
- Viewport search (`Ctrl+Shift+F`) over visible lines and scrollback, with match navigation. The scan is chunked across ticks so a full-history search never stalls the UI.
- Linux selection model: primary selection on drag, middle-click paste, separate `Ctrl+Shift+C` / `Ctrl+Shift+V` clipboard, sanitized bracketed paste.
- One binary serving the GUI, `mutiny ctl`, and `mutiny mcp`. 21 IPC verbs, 17 MCP tools, 4 MCP resources, held at parity by tests.
- Carrier-assigned consent with kernel-verified peer identity, and a durable redacted audit trail.
- TOML config with live reload for font, colors, palette, geometry, and scrollback depth. See [Configuration](#configuration).
- Terminal capability queries answered (XTVERSION, OSC 10/11 colors, DCS terminfo), so modern shells like fish 4 start without blocking.
- IME input through GtkIMContext: CJK, dead keys, and compose sequences, with preedit shown at the cursor. Font fallback for CJK, emoji, and box-drawing glyphs.

Not built yet:

- No config editor or in-app settings UI; the TOML file is the only surface.
- No shell integration for shells other than bash, zsh, fish, and nushell. dash and plain sh get no markers.
- Split ratios are stored but not applied; splits render at the GTK default and drags are not persisted.
- No Kitty graphics, no drag-to-detach gestures, no OSC 777 notifications, no IME or font fallback chain.
- Rendering repaints every cell. The damage-tracking API exists but is unwired.
- No session restore. There is no multiplexer daemon, so a GUI crash kills every shell. `tmux` and `zellij` compose alongside rather than being reimplemented.

[VISION.md](VISION.md) is the full product and architecture definition, including a growth diagnostic that says which claims are ahead of the code.

## Why AGPL

mutiny is AGPL-3.0-or-later, and the choice follows from what the program is.

A terminal is increasingly reached over a network rather than installed locally. You drive it from another machine over SSH or a tunnel, and an agent drives it through an MCP endpoint. The GPL's source obligation triggers on distribution. It does not trigger when someone runs a modified version as a service and lets you interact with it remotely. The AGPL closes that gap: run a modified mutiny and let users interact with it over a network, and you must offer them the source of your version.

That gap matters here because the surface is shared. The person watching the panes and the agent acting on them use the same program, and the consent ladder and audit trail are what keep that arrangement honest. A hosted variant that quietly removed the consent bar or the audit log, and kept the change private, would leave the person on the other end of the tunnel with no way to know. Copyleft on network use keeps the guarantee attached to the program rather than to one build of it.

It pairs with the code rule in VISION.md: dependencies come from crates.io and standard Linux system libraries, and every merged change must be explainable unaided by a human maintainer. Source you can read is only useful if you can get it.

## Relationship to Haven

[Haven](https://github.com/GlassHaven/Haven) is a mobile thin-client workspace. mutiny is its desktop counterpart. In Haven's vocabulary mutiny provides two of the four primitives on the workstation: Runtime, a place to run things, and Presence, the surface in both directions. Haven's Namespace and Gateway reach mutiny over SSH and WireGuard. mutiny does not reimplement them.

The shared contract is that a human and an agent use one vocabulary. Every action in the menu bar, command palette, and keybind cheatsheet comes from a single action registry and is also callable through `mutiny ctl` and `mutiny mcp`. An agent reads the same workspace state that is rendered on screen, and its calls land in an audit trail and show up in the window chrome. There is no agent-only bypass layer.

mutiny ships no model, no chat client, and no API keys, and it names no vendor's agent CLI. Any client speaking standard PTY and JSON-RPC conventions is a peer. Haven is the reference peer because the two projects share the presence model. mutiny runs standalone and does not depend on it.

## Build from source

Requirements:

- Rust 1.92 or newer (edition 2024)
- GTK 4.22 or newer, with development files and `pkg-config`
- A C linker and `make` for the `-sys` crates

Check your GTK version first. The crate enables the `gtk4-rs` `v4_22` feature and will not build against anything older:

```sh
pkg-config --modversion gtk4   # must be 4.22 or higher
```

Arch carries gtk4 4.24 and Ubuntu 26.04 carries 4.22. Ubuntu 24.04 ships 4.14 and is too old.

```sh
# Arch
sudo pacman -S --needed base-devel gtk4 rustup
rustup default stable

# Ubuntu 26.04 or newer
sudo apt install build-essential pkg-config libgtk-4-dev
# then install Rust via rustup if your distro's rustc is older than 1.92
```

Build and run:

```sh
cargo build --release
./target/release/mutiny
```

Or install into `~/.cargo/bin`:

```sh
cargo install --path .
# or, without a checkout:
cargo install --git https://github.com/GlassOnTin/mutiny
```

At runtime mutiny needs the GTK 4.22+ shared libraries and a Wayland or X11 session. Release binaries built on Arch are dynamically linked against the build host's GTK and glibc, so building from source is the reliable path on other distributions.

## Running it

One binary, three entry points:

```sh
mutiny                          # GUI, binds both control sockets
mutiny ctl list                 # human and script CLI, JSON output
mutiny mcp --consent ask        # stdio JSON-RPC 2.0 MCP server for an agent
```

Two Unix sockets, because one cannot serve both trust paths:

- Operator: `$XDG_RUNTIME_DIR/mutiny.sock`. `mutiny ctl` connects here. A same-uid peer is the operator.
- Agent: `mutiny-agent.sock` beside it. `mutiny mcp` connects here by default.

Override the operator path with `--socket` or `MUTINY_SOCKET`; the agent path is derived from it. Launching a second GUI against a live operator socket forwards a new-window request to the first instance and exits.

The GUI flag `--agent-consent ask|paired|observe-only` (default `ask`) pins the mode the agent carrier assigns. It is an operator-side grant. A bridge cannot raise it.

Identifiers are stable regardless of the public product name, which is still open: application id `dev.mutiny.Terminal`, environment `TERM_PROGRAM=mutiny` and `MUTINY_PANE_ID`, MCP resources under `ui://mutiny/*`.

## Configuration

Config lives at `$XDG_CONFIG_HOME/mutiny/config.toml`, falling back to `~/.config/mutiny/config.toml`. The file is optional: a fresh install runs on the defaults below and never writes a config file itself.

Precedence is **CLI flag, then config file, then built-in default**. The flags `--font`, `--font-size`, `--cols`, and `--rows` shadow the matching keys, and they are re-applied on every live reload, so editing the file cannot discard a flag you launched with.

Every key, with its default:

```toml
[font]
family = "Monospace"
size = 11                      # points, clamped 6..72

[colors]
background = "#16181d"
foreground = "#e6e9ef"
cursor = "#89b4fa"
selection = "#3b5278"
accent = "#5c9ef5"             # active-pane top bar
palette = [                    # exactly 16 entries, ANSI 0-15
  "#1e222a", "#e06c75", "#98c379", "#e5c07b",
  "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
  "#5c6370", "#f38ba8", "#a6e3a1", "#f9e2af",
  "#89b4fa", "#cba6f7", "#94e2d5", "#ffffff",
]

[geometry]
initial_cols = 100             # grid a new pane spawns at, clamped 20..500
initial_rows = 30              # clamped 5..200
window_width = 980             # new windows, clamped 320..7680
window_height = 640            # clamped 200..4320
padding_px = 6.0               # clamped 0..64

[terminal]
scrollback_lines = 10000       # capped at 1000000
```

Colors are `#rrggbb`. The 256-color cube and grayscale ramp beyond ANSI 16 are computed, not configurable.

**Live reload.** The file is stat'd every 250 ms and reloaded when it changes. Font, colors, palette, and padding apply immediately to every open pane: cell metrics are recomputed, which resizes the grid and the PTY. Geometry and scrollback depth apply to *newly created* panes and windows, because alacritty fixes scrollback at construction and resizing existing windows under you would be rude. Deleting the file reverts to defaults.

**Errors are survivable.** Unknown keys are rejected so a typo is reported rather than silently ignored. A malformed file leaves the previous config in place and warns, and does not retry until the file changes again. An invalid color falls back to that key's default; a palette that is not exactly 16 entries falls back to the whole default palette. Out-of-range numbers are clamped. None of these stop the terminal.

### Keybinds

| Action | Chord |
| --- | --- |
| New window / new tab / close pane | `Ctrl+Shift+N` / `T` / `W` |
| Split right / split down | `Ctrl+Shift+R` / `E` |
| Detach split to window | `Ctrl+Shift+D` |
| Focus next / previous pane | `Ctrl+Shift+]` / `[` |
| Jump to next urgent pane | `Ctrl+Shift+A` |
| Jump to previous / next OSC 133 prompt | `Ctrl+Shift+Up` / `Down` |
| Copy / paste | `Ctrl+Shift+C` / `V` |
| Search scrollback | `Ctrl+Shift+F` |
| Toggle menu bar | `Ctrl+Shift+M` |
| Command palette | `Ctrl+Shift+P` |
| Agent activity and audit log | `Ctrl+Shift+J` |
| Keybind cheatsheet | `Ctrl+Shift+?` |

The cheatsheet and the palette are generated from the same action registry as the menus and the IPC dispatch, so they cannot drift apart.

## Consent ladder

The tier is a property of the verb, not the caller. A human running `mutiny ctl` is the consent. An agent calling the same verb raises a prompt.

| Tier | Meaning | Status |
| --- | --- | --- |
| 1 | Observe. Read-only tools and resources, no prompt. | shipped |
| 2 | Act once. One-shot writes, per-action consent in window chrome. | shipped |
| 3 | Standing policy. A scoped, rate-capped, expiring grant installed once. | planned (M2) |
| 4 | Irreversible. Always per-action, never standing. | planned |

Consent mode is assigned by the carrier that accepted the connection, never claimed by the client. An envelope carrying `consent_mode` is refused. Peer identity comes from the kernel via `SO_PEERCRED`, and a peer whose uid differs from the server's is refused outright.

Session grants bind to a kernel-verified peer identity (uid, pid, and process start time), not to the client label. The label is display and audit text and is not an input to the consent decision, so claiming another client's name cannot inherit its grant. A grant lives as long as the granted process: restarting an MCP bridge requires re-approval.

Every decision appends to a redacted, durable audit trail at `$XDG_STATE_HOME/mutiny/audit.jsonl`, recording the verb, its tier, the carrier, the peer identity, and the decision. Text payloads are logged as byte counts, never content. `Ctrl+Shift+J` opens the viewer, where session grants can be revoked.

Ask mode fails closed when no GUI window is open to present a prompt. Consent chrome is not keyboard-reachable and no action verb resolves a consent, so input injection cannot confirm its own permission.

One limit is worth stating plainly: same-uid processes are mutually trusted under the Unix model. Any process running as you can reach the operator socket and act as the operator. The uid check closes the cross-user hole, not that one.

## Development

```sh
cargo test                    # 71 tests, no display required
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo bench                   # headless engine baseline
./scripts/ci.sh               # everything CI runs, forced displayless
```

The suite runs headless because `AppController` works without a GTK application attached, so engine, topology, IPC, and MCP behavior are all testable without a desktop. CI additionally runs it under Xvfb so the GTK-dependent assertions execute too.

[benches/BASELINE.md](benches/BASELINE.md) records the measured engine baseline. One finding from it shaped the search design: a full 10 000-line scrollback scan with no match costs about 3.2 ms against the 4 ms GUI tick. Search is therefore chunked at 512 lines per tick, which measures 233 us worst case, and a full-history miss spreads over 20 ticks instead of blocking one.

[CLAUDE.md](CLAUDE.md) documents the architecture for contributors and coding agents, including the invariants that are easy to break: state lives outside widgets, one thread and one 4 ms tick, and the OSC side channel slices the byte stream so markers see the exact cursor position.

## License

AGPL-3.0-or-later. Full text in [LICENSE](LICENSE), copyright notice in [NOTICE](NOTICE).

Copyright (C) 2026 GlassOnTin.
