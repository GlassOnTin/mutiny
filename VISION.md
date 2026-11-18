# Mutiny (working title): Product & Architecture Definition

Status: draft v3, 2026-11-18. Owner: Ian. Public name remains open until first public release.

## Thesis

A GPU-accelerated Linux desktop terminal written in pure Rust and pure `gtk4-rs`, built for KDE Plasma and GTK desktops without upstream code, Zig toolchains, or C-ABI baggage from Ghostty. Mutiny is designed as the workstation peer to distributed, AI-native environments like Haven (`~/Code/Haven/VISION.md`): humans and agents share the same observable terminal surfaces, act through the same verb vocabulary, and compose across local shells, SSH/WireGuard tunnels, and MCP tool buses with the human keeping the wheel.

Six architectural commitments govern the codebase from line one:

1. **One committed pure-Rust VT engine (`alacritty_terminal`), zero Ghostty legacy.** No dual-backend trait facade and no foreign C library. Protocol extensions missing from `alacritty_terminal` (OSC 133 command zones, OSC 7 working directory, OSC 9;4 progress, output-quiescence turn tracking, and later Kitty Graphics) live in a stream-level Rust `PaneEngine` wrapper. Sixel is cut from Tier 0.
2. **Off-widget `PaneArena` and reparent-safe GPU rendering.** Terminal state, PTYs, and scrollback live in an application-owned arena keyed by `PaneId`, never inside GTK widgets. Widgets are disposable viewports rendered through GTK 4.22's Vulkan/NGL `GtkSnapshot` (`GskRenderNode`) pipeline rather than per-pane `GLArea` contexts, so detaching or moving splits across windows never tears down GPU state or stalls PTYs.
3. **First-class KDE and GTK citizenship (`gtk4-rs` without `libadwaita`).** Server-side decorations (`xdg-decoration` on KWin) are respected natively so Breeze titlebars and borders work out of the box. A toggleable horizontal `PopoverMenuBar` (`Ctrl+Shift+M`), D-Bus `GMenuModel` export for KDE Global Menu, and persistent tab-bar controls remain visible regardless of CSD or SSD mode.
4. **Shared human-and-agent presence (Haven peer contract).** Every surface obeys Haven's three presence laws:
   - *Observable state:* structured pane snapshots (cursor, modes, OSC metadata, semantic command output), scrollback search, and a ring of the last 8 exited panes (`read-exited`, so fast-failing sessions remain diagnosable) are exposed both over IPC and as addressable `ui://mutiny/*` MCP resources.
   - *Actionable API:* `send-input` separates UTF-8 text, bracketed-paste framing, and raw named keys (`enter`, `esc`, `ctrl-c`, arrows) so raw-mode agent REPLs never mangle multi-line turns; `send-to-agent` and `await-turn` support both OSC 133 shell prompts and output-quiescence settle windows for raw-mode TUIs.
   - *Reachable surface & consent:* agents can post inline operator notices (`present-notice`) that light up `urgent-next` (`Ctrl+Shift+A`). Every MCP call is tagged with a verb-level consent tier (`Observe` free, `Act` gated by interactive in-window consent bar or scoped session policy), surfaced via a live `Agent` activity pill, and recorded in an inspectable audit log (`Ctrl+Shift+J`). Keyboard input injection never confirms a consent bar.
5. **Native MCP server alongside Unix socket IPC, vendor-neutral by rule.** The single `mutiny` binary serves both `mutiny ctl` (human/script CLI) and `mutiny mcp` (JSON-RPC 2.0 MCP server over stdio, tunnelable from Haven over SSH or WireGuard). Mutiny ships no LLM, no chat pane, no API keys, and zero vendor-specific agent branding.
6. **Sustainability by construction.** One declarative `ActionSpec` table generates the keybind router, `PopoverMenuBar`, right-click menu, command palette, keybind cheatsheet, and IPC/MCP action dispatch so discoverability and scriptability cannot drift.

## Primary user

A Linux desktop power user on Wayland or X11 (KDE Plasma or GTK desktop) who runs concurrent local shells, remote sessions, and AI agents, including cross-device workflows driven from or alongside Haven. macOS and Windows are out of scope.

## Principles

1. **Linux desktop is the flagship.** Wayland-first with X11 support. KDE SSD and GTK CSD both behave like native citizens.
2. **1:1 verb parity by construction.** Menu bar, command palette, auto-generated keybind cheatsheet, right-click menu, `mutiny ctl`, and `mutiny mcp` read from one `ActionSpec` registry.
3. **No silent automation, no divergent state.** Agent reads and writes operate on the exact `Workspace` state rendered on screen, leave an audit trail, and surface visibly in window chrome.
4. **Corporate and vendor neutrality.** Any CLI or MCP client speaking standard PTY and JSON-RPC conventions is a first-class peer; vendor names never appear in product chrome or documentation.
5. **Pure Rust, human-understandable code.** Dependencies come from `crates.io` and standard Linux system libraries (`gtk4`, `pango`, `harfbuzz`, `fontconfig`). Every merged change must be explainable unaided by a human maintainer.
6. **Local-first.** Plain TOML config with live reload, zero telemetry, zero outbound network calls.

## Feature tiers

### Tier 0.1 (M0 / M0.5 "Self-Hosting Rebel")

- Pure `gtk4-rs` (`v4_22`) windowing with native KWin SSD support, toggleable `PopoverMenuBar` (`Ctrl+Shift+M`), command palette (`Ctrl+Shift+P`), keybind cheatsheet (`Ctrl+Shift+?`), and agent audit viewer (`Ctrl+Shift+J`).
- `PaneEngine` over `alacritty_terminal 0.26`: PTY lifecycle, line-level damage tracking, 5-flag Kitty keyboard protocol, OSC 8 hyperlinks, OSC 52 clipboard, OSC 7 cwd tracking, OSC 9;4 progress, OSC 133 (`A`/`B`/`C`/`D`) command journal, and output-quiescence turn tracking.
- Linux selection model: primary selection on mouse drag, middle-click paste, distinct `Ctrl+Shift+C` / `Ctrl+Shift+V` clipboard, and sanitized bracketed paste.
- Binary split trees and tabs backed by `PaneArena`: `split-right`, `split-down`, `detach-split` (`Ctrl+Shift+D`), `move-pane`, `urgent-next` (`Ctrl+Shift+A`), and an 8-slot exited-pane snapshot ring (`read-exited`).
- Canonical Unix socket IPC (`$XDG_RUNTIME_DIR/mutiny.sock`) + `mutiny ctl` CLI + `mutiny mcp` (stdio MCP server exposing `ui://mutiny/workspace`, `ui://mutiny/pane/{id}/snapshot`, `ui://mutiny/exited`, `ui://mutiny/audit`, and read/act/turn/reach-back tools with `--consent ask|paired|observe-only`).

### Tier 0.2 (Remaining Tier 0 polish, built inside Mutiny)

- Kitty Graphics Protocol image placements in `PaneEngine` + `GtkSnapshot` texture nodes, plus inline agent media overlays (`present-media`).
- Drag-to-reorder and drag-to-detach gestures layered onto the keyboard mobility verbs.
- OSC 777 desktop notifications wired to desktop notification portals and tab badges.
- Bundled shell integration snippets (`OSC 133` + `OSC 7`) for `bash`, `zsh`, `fish`, and `nushell`.
- Interactive viewport search bar over `PaneEngine::search_scrollback`.

### Tier 1

- Session restore and named workspace bundles (matching Haven's `save_workspace` / `compose_workspace` pattern): window/tab/split geometry, working directories, and serialized scrollback history, without running a background multiplexer daemon.
- Quick terminal: Yakuake-style drop-down via `gtk4-layer-shell`, toggled over IPC/MCP or global shortcut.
- Authenticated streamable-HTTP MCP listener bound to explicit WireGuard / SSH-tunneled interfaces with pairing tokens and rate-capped Tier-3 standing policies.

## Architecture Overview

```text
mutiny (single Rust binary: GTK4 GUI + `mutiny ctl` CLI + `mutiny mcp` server)
├── action      Single ActionSpec registry + ConsentTier -> keybinds, GMenuModel, palette, cheatsheet
├── engine      PaneEngine: alacritty_terminal 0.26 + PTY + OSC 133/7/9;4 + quiescence + named keys
├── topology    Widget-free Workspace: PaneArena + SplitNode + urgent_next + 8-slot ExitedPane ring
├── ipc         JSONL over $XDG_RUNTIME_DIR/mutiny.sock + consent modes + redacted AuditEntry ring
├── mcp         Stdio JSON-RPC 2.0 MCP server (`tools/*` + `ui://mutiny/*` resources) over ipc
└── ui          Pure gtk4-rs (v4_22): MutinyWindow (SSD chrome, consent/notice bars, audit view) + TerminalView
```

## Non-goals

- No built-in terminal multiplexer daemon; `tmux` and `zellij` compose alongside `mutiny ctl` and `mutiny mcp`.
- No bundled AI model, chat client, prompt language, or vendor-specific agent integration.
- No `libadwaita` dependency, forced client-side headerbars, or GNOME-only chrome assumptions.
- No Ghostty code, Zig build steps, or C-ABI core bindings.
- No Sixel support in Tier 0.
- No macOS or Windows targets, telemetry, or parallel GUI config store.
