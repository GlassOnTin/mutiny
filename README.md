# mutiny

[![CI](https://github.com/GlassOnTin/mutiny/actions/workflows/ci.yml/badge.svg)](https://github.com/GlassOnTin/mutiny/actions/workflows/ci.yml)
[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue.svg)](LICENSE)

A Linux desktop terminal in pure Rust and pure `gtk4-rs`. Panes move between splits, tabs, and windows without restarting their shells, and an agent can read and drive the same panes a human is watching, through the same verbs.

Wayland-first with X11 support, built for KDE Plasma and GTK desktops. No libadwaita, no C-ABI terminal core, no Zig toolchain. Server-side decorations are left to KWin, so Breeze titlebars work as they do for any other Qt or GTK application.

## Status

Early, and honest about it. M0, M0.5, M1, M2 (save for an unscheduled system-wide global shortcut), and M3 (`0.2.0` Paired-Limb cycle: M3.1 bilateral peer pairing, M3.2 mirrored authentication and HMAC-signed cross-limb consent, M3.3 focus-aware bidirectional notifications, and M3.4 unified constellation + paired-Haven session tree, cross-limb pane handoff, and symmetric media/web/clipboard throw) are shipped: session restore (including re-attaching multiplexer sessions in order), the authenticated HTTP MCP listener with symmetric `initialize` tap-to-pair and durable `peers.json` (`0600`, SHA-256 inbound hashes only), the outbound `HavenPeerClient` with HMAC-SHA256 cross-limb consent co-signing, focus-aware notification mirroring (`CompositeNotifier` + burst-5 `TokenBucket`) and unfocused `OSC 133;D` long-command alerts, the `!haven` Limb Status Pill and Haven Alerts inbox, the inline Haven stalled SSH Auth Bar, Operator-only `insert-peer-totp` (`Ctrl+Shift+Y`), the hierarchical `session-tree` picker (`Ctrl+Shift+S`) and one-click tiled `open-session-grid`, cross-limb `handoff-to-peer` (`Ctrl+Shift+H`), symmetric `present-to-peer` (`present_media`, `present_web`, `write_clipboard`), the Settings dialog, Tier-3 standing policies, `PolicyChange` and Tier-4 irreversible per-action gating, and the summonable scratch quick terminal all work today. The engine, pane mobility, peer pairing, cross-limb auth/consent/notifications/handoff, and agent surface are covered by 241 tests. Most run with no display at all; the GTK widget tests run under Xvfb in CI. GPU compositing to a physical Wayland output is still not exercised by the suite.

Working today:

- `alacritty_terminal` 0.26 engine with a PTY per pane, 10 000 lines of scrollback, and line-level search.
- OSC 133 command journal, host-checked local OSC 7 working directory, OSC 9;4 progress state, OSC 52 clipboard (OSC 8 hyperlinks are parsed by the VT engine; click/hover UI is not built yet).
- Shell integration bundled for bash, zsh, fish, and nushell, injected automatically at spawn. A fresh install gets prompt jumping, command-output capture, and exit-code reporting without editing any dotfile.
- Binary split trees and tabs. The tab strip wraps to multiple rows within the window width and scrolls past a height cap, so opening many tabs never stretches the window. `detach-split` pops a pane into its own window and `move-pane` moves it into another tab, both without touching the running shell or its scrollback. Divider positions are stored, applied, and written back on drag; tabs drag to reorder within a window, drag out of Mutiny (or onto their own terminal area) to tear off into a new window, and drag onto another Mutiny window's tab strip or terminal area to redock across windows.
- `urgent-next` jumps across windows, tabs, and splits to the next pane needing attention (a bell, a failed or unfocused long command, or an agent notice), or opens the Limb Popover inbox when unread `!haven` peer alerts are waiting.
- A ring of the last eight exited panes keeps the final screen and exit code, so a session that fails on startup is still diagnosable.
- Viewport search (`Ctrl+Shift+F`) over visible lines and scrollback, with match navigation. The scan is chunked across ticks so a full-history search never stalls the UI.
- Linux selection model: primary selection on drag, middle-click paste, separate `Ctrl+Shift+C` / `Ctrl+Shift+V` clipboard, sanitized bracketed paste.
- Mouse reporting and wheel zoom: when a program asks for mouse events (tmux with `mouse on`, vim, less), the wheel and clicks go to it instead of mutiny's scrollback and selection; hold Shift to override back to local scrollback and selection, and hold `Ctrl` while scrolling the mouse wheel (over the terminal or the tab strip) to scale both the terminal font and session tab labels (`Ctrl+0` resets to the configured size).
- One binary serving the GUI, `mutiny ctl`, and `mutiny mcp`. 47 IPC verbs, 43 MCP tools, 31 GUI actions, 4 static MCP resources plus a per-pane snapshot resource, held at parity by tests.
- Carrier-assigned consent across Tiers 1 through 4 plus `PolicyChange` with kernel-verified peer identity, Tier-3 standing policies, per-action `PolicyChange` (`install-policy`, `revoke-policy`, `pair-haven`, `enroll-haven-signer`, `revoke-peer`) and Tier-4 (`end-session`, `quit`) gating, and a durable redacted audit trail.
- Bilateral peer pairing, mirrored authentication, focus-aware bidirectional notifications, and cross-limb session handoff (`peers.json`, mode `0600`): an authenticated HTTP MCP listener with symmetric `initialize` tap-to-pair (inbound tokens stored only as SHA-256 hashes at rest), an outbound stdlib `HavenPeerClient` over loopback or WireGuard/Yggdrasil tunnels, `list-peers` / `pair-haven` / `enroll-haven-signer` / `revoke-peer` / `list-peer-alerts` / `clear-peer-alerts` / `session-tree` / `handoff-to-peer` / `present-to-peer`, unfocused cross-limb HMAC-SHA256 consent co-signing on Haven across Tiers 2 through 4 (with strict one-shot enforcement on `PolicyChange` and Tier-4 `Irreversible`), an inline GTK4 Haven Auth Bar for stalled phone SSH prompts (physical widget interaction only), `insert-peer-totp` (`Ctrl+Shift+Y`, local Operator/GUI only, typed straight into the PTY without touching the clipboard), focus-aware outbound notification relay (`OSC 777`, local `present-notice`, and unfocused `OSC 133;D` long commands with per-peer burst-5 `TokenBucket` rate limiting and zero scrollback leakage), a chrome Limb Status Pill (`!haven:<label>` badge + Haven Alerts inbox, with zero unprompted `list_notifications` calls by default and opt-in `inbound_shade_allowlist` polling), one-action mux session handoff to Haven (`Ctrl+Shift+H`), and symmetric media/web/clipboard presentation on Haven (`present-to-peer`). See [Remote agents over HTTP](#remote-agents-over-http).
- TOML config with live reload for font, colors, palette, geometry, scrollback depth, and notification thresholds. See [Configuration](#configuration).
- Terminal capability queries answered (XTVERSION, OSC 10/11 colors, DCS terminfo), so modern shells like fish 4 start without blocking.
- OSC 777 notifications and unfocused `OSC 133;D` long-command completions reach the desktop via `org.freedesktop.Notifications`, badge the notifying tab, and mirror to paired Haven limbs when Mutiny is unfocused.
- Damage-driven redraw: only changed rows are re-rendered. An idle frame is about 19x cheaper than a full repaint, measured under Xvfb.
- Text is placed one glyph cluster per cell, so the drawn cursor sits exactly on the emulator's grid cursor instead of drifting past the end of the line.
- Session restore: layout, split ratios, working directories, and scrollback survive a restart. See [Session restore](#session-restore).
- A hierarchical session picker (`Ctrl+Shift+S`, `mutiny ctl session-tree`) groups local tmux/zellij sessions (nesting organ sessions under `[sessions] root_session = "meta"` with active, busy `OSC 133;C`, and `!attention` rollups), configured SSH hosts, and paired Haven limb sessions and hardware/audio/USB bridges, plus one-click tiled multi-pane grid windows (`open-session-grid`). See [Multiplexer sessions](#multiplexer-sessions).
- IME input through GtkIMContext: CJK, dead keys, and compose sequences, with preedit shown at the cursor. Font fallback for CJK, emoji, and box-drawing glyphs.
- Kitty graphics images render inline as texture nodes, and a `present-media` verb lets an agent place an image in a pane. See [Inline images](#inline-images).
- Summonable scratch quick terminal (``Ctrl+Shift+` ``) that hides on dismiss or window-manager close while keeping its shell alive, plus an in-app Settings dialog (`Ctrl+Shift+,`) covering every key in `config.toml`.

Not built yet:

- The Settings dialog writes `config.toml` on Save rather than validating per key live, and Tier-3 standing policies live in memory rather than persisting across restarts.
- No click/hover UI for OSC 8 hyperlinks or plain-text URLs, and no visual indicator yet for OSC 9;4 progress.
- No shell integration for shells other than bash, zsh, fish, and nushell. dash and plain sh get no markers.
- Restored scrollback is plain text: colors and attributes are not preserved, and a crash loses changes since the last clean save.
- Drag gestures work, but their GTK wiring is not covered by tests (the model operations they call are).
- Kitty graphics is a slice: no animation, file/shared-memory transmission, or unicode placeholders.
- Render measurement covers node construction under Xvfb, not GPU compositing or presentation on a real compositor.
- Mutiny-owned shells are not restored. There is no multiplexer daemon, so a crash kills them; session restore brings back layout and history with fresh shells. A pane attached to tmux or zellij re-attaches, because those processes belong to the multiplexer. `tmux` and `zellij` compose alongside rather than being reimplemented.
- A pane attached to tmux or zellij gets no shell integration, so prompt jumping and command capture do not work inside it.
- The session picker scans local tmux and zellij plus configured SSH hosts. screen, abduco, and dtach are not discovered.
- The HTTP MCP carrier is a slice: no SSE and no session management. Peer identity, session grants, and consent mode are all per token. Driven end to end from Haven on a phone over both an SSH forward and a real WireGuard tunnel.

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
mutiny install-desktop          # app icon + launcher into ~/.local/share
mutiny ctl save-session         # snapshot the current layout and session list now
mutiny ctl quit                 # save, then quit
mutiny ctl close-tab            # close a tab (--window/--tab to target)
mutiny ctl close-window         # close a window and its panes
mutiny ctl list                 # human and script CLI, JSON output
mutiny mcp --consent ask        # stdio JSON-RPC 2.0 MCP server for an agent
```

Two Unix sockets, because one cannot serve both trust paths:

- Operator: `$XDG_RUNTIME_DIR/mutiny.sock`. `mutiny ctl` connects here. A same-uid peer is the operator.
- Agent: `mutiny-agent.sock` beside it. `mutiny mcp` connects here by default.

Override the operator path with `--socket` or `MUTINY_SOCKET`; the agent path is derived from it. Launching a second GUI against a live operator socket forwards a new-window request to the first instance and exits.

The GUI flag `--agent-consent ask|paired|observe-only` (default `ask`) pins the mode the agent carrier assigns. It is an operator-side grant. A bridge cannot raise it.

Identifiers are stable regardless of the public product name, which is still open: application id `dev.mutiny.Terminal`, environment `TERM_PROGRAM=mutiny` and `MUTINY_PANE_ID`, MCP resources under `ui://mutiny/*`.

### Remote agents over HTTP

Nothing listens on the network unless you ask for it. `--http-bind` names an explicit interface address, normally a WireGuard or SSH-tunneled IP:

```sh
mutiny --http-bind 100.64.0.5:18740 --agent-consent ask
```

A wildcard bind (`0.0.0.0` or `[::]`) is refused, because the listener is meant to sit on a named interface rather than every interface. By default `--http-bind` enables symmetric `initialize` tap-to-pair (pass `--no-http-pair` to disable it): an unauthenticated `POST /mcp` carrying `method: "initialize"` raises a single inline Pairing Bar in the window (concurrent requests receive HTTP `429`), lets you pick the peer's `ConsentMode`, mints an `mty_...` bearer token in `result._meta["sh.mutiny/pairingToken"]`, and stores only its SHA-256 digest in `$XDG_CONFIG_HOME/mutiny/peers.json` (`0600`). Any non-`initialize` unauthenticated request fails immediately with `401`. You can also pair an outbound Haven phone endpoint or manage peers from the CLI or Session menu:

```sh
mutiny ctl pair-haven --label pixel --url http://127.0.0.1:8740/mcp --enroll-signer
mutiny ctl enroll-haven-signer --id-or-label pixel
mutiny ctl list-peers
mutiny ctl session-tree --refresh
mutiny ctl handoff-to-peer
mutiny ctl present-to-peer --path /abs/path/chart.png --caption "Latency"
mutiny ctl list-peer-alerts
mutiny ctl clear-peer-alerts
mutiny ctl revoke-peer --id-or-label pixel
```

Pick a port that is not already taken. Haven's own MCP reverse tunnel binds `127.0.0.1:8730` on the workstation, so that port is a poor choice for the peer it is meant to serve. If the bind fails the listener stays down and the GUI keeps running, with a warning naming the reason.

Clients POST JSON-RPC to `/mcp` with `Authorization: Bearer <token>`:

```sh
curl -s http://100.64.0.5:18740/mcp \
  -H 'Authorization: Bearer 9f2c...' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

An unpaired client gets `401` before its body is read, so it cannot make the listener allocate or forward anything. Only `POST /mcp` is served; anything else is `404`.

**A network client can never be assigned Operator.** The listener forwards to the agent socket only, and the agent carrier's mode is pinned at GUI launch by `--agent-consent`. This module has no path to the operator socket at all, so the guarantee is structural rather than a check that could be forgotten. A `consent_mode` field in the JSON-RPC body is ignored, exactly as it is refused on the Unix envelope.

This is a bounded slice. Each request is stateless: there is no MCP session id, so the name sent in `initialize` does not persist and every call is audited under the peer's configured label (or `http-peer` when `--http-token` is used without `--http-peer`). SSE streaming, session management, and `DELETE` are not implemented.

HTTP has no `SO_PEERCRED` equivalent, so the pairing token is the peer credential. Name each peer and give it its own token:

```sh
mutiny --http-bind 10.1.0.4:18740 \
  --http-peer phone=9f2c... \
  --http-peer laptop=4b71... \
  --agent-consent ask
```

Each peer gets its own relay carrier and its own identity, derived from its token. The audit trail records that identity rather than mutiny's pid, so two devices are distinguishable, and session grants key on it: an Allow Session tap for one peer does not cover another. A peer cannot choose its identity, only present a token; the mapping is fixed by your configuration at launch. `--http-token` still works for a single unnamed peer.

Each peer also carries its own consent mode, so one device can be trusted while another is read-only:

```sh
mutiny --http-bind 10.1.0.4:18740 \
  --http-peer laptop=4b71...:paired \
  --http-peer tablet=8c02...:observe-only \
  --http-peer guest=1f9a...:ask
```

Without a `:MODE` suffix a peer inherits `--agent-consent`, so existing invocations keep working. `operator` is not assignable to a peer and is refused at the flag, because it belongs to the operator socket where a same-uid human is the consent. The startup line reports each peer's mode, for example `for peers: laptop(paired), tablet(observe-only)`.

#### Driving it from Haven

Verified end to end with Haven on a phone, over an SSH local forward rather than a directly exposed port. Bind loopback only on the workstation:

```sh
mutiny --http-bind 127.0.0.1:18740 --agent-consent paired
```

Then forward the phone's loopback to it through the SSH profile for that host (`add_port_forward`, type `LOCAL`, bind port 18740 to `127.0.0.1:18740`). Haven's Linux guest shares its network namespace, so from the guest:

```sh
curl -s http://127.0.0.1:18740/mcp \
  -H 'Authorization: Bearer <token>' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_workspace","arguments":{}}}'
```

What that initial M2 run showed (against the 18-tool surface at that milestone; the surface has since grown to 43 tools): no token and a wrong token both `401`, the right token `200`, `initialize` negotiating `2025-06-18`, all 18 tools listed, a command sent with `send_terminal_input` and its output read back with `read_screen_text`, `spawn_pane` creating a second pane, and `present_notice` reaching the desktop. Restarting with `--agent-consent observe-only` allowed reads and denied both Act verbs, and the denied command never ran. The audit recorded `carrier=paired` then `carrier=observe-only` with `denied` decisions, and never `operator`.

Every one of those single-token calls audited against the mutiny process's own pid, which was the shared-identity gap before `--http-peer` added per-token peer identities.

The same run was repeated over WireGuard, with the listener bound to the workstation's own tunnel address rather than loopback:

```sh
mutiny --http-bind 10.1.0.4:18740 --agent-consent paired
```

The phone, a peer on the same gateway-hosted WG server, reached `http://10.1.0.4:18740/mcp` directly with no SSH forward involved. Unpaired and wrong-token requests got `401`, the paired token got `200`, all 18 tools at that milestone and all 6 resources listed (4 static plus one snapshot per live pane), a command sent from the phone executed on the desktop, and `spawn_pane` added a second pane. The audit again showed `carrier=paired` and never `operator`.

A split tunnel is enough: routing only the tunnel subnet to the WG interface leaves the default route alone, so nothing else on the workstation changes.

## Menus and labels

Five menus, each an archetype with no overlap. Every action lives in exactly one:

| Menu | Covers | Actions |
| --- | --- | --- |
| **Session** | sessions, top-level windows, and layout | New Session Tab, New Session, Open Session (submenu), Open Session Grid \| New Window, Quick Terminal, Save Session Layout \| Close Tab, Close Window |
| **Edit** | the focused pane's content, and settings | Copy, Paste \| Search, Previous/Next Command \| Settings |
| **Pane** | pane structure and focus | Split Right/Down, Move Pane to New Window \| Close Pane \| Focus Next/Prev, Next Urgent |
| **Haven** | cross-limb actions, pairing, TOTP, and alerts | Hand Off Pane to Haven, Present to Haven, Insert Haven TOTP Code \| Pair Haven Limb, Clear Haven Alerts |
| **View** | chrome and introspection | Menu Bar, Command Palette, Agent Activity, Keybind Cheatsheet |

The old Shell / Panes / View split overlapped: "Shell" held pane closing, "View" held session attaching, and "Panes" mixed structure with content navigation. Help existed for one item, which now sits in View. Action slugs and keybinds are unchanged; `tests/menu_ia.rs` holds a golden list so a rename or rebound chord fails a test.

**New Tab follows `preferred`.** With `tmux` or `zellij` it creates a new session named with the smallest free `mutiny-N`; with `none` it opens a plain shell, which is the old behaviour. The `spawn-tab` IPC verb deliberately stays a plain shell, so agents and scripts are unaffected.

**New Session** (Session menu, tab menu, or palette) asks for a name and lets you pick tmux, zellij, or none per session, overriding the config default. Creating fails if the name is taken rather than attaching to it; use Open Session for that.

**Closing a view is not ending a session.** With a multiplexer these are different operations, and the wording says so rather than leaving you to guess:

| Action | What ends | What survives |
| --- | --- | --- |
| Close Tab / Close Pane / Close Window | this terminal client | the session and everything running in it |
| End Session | the session itself | nothing inside it, on any device |

End Session sits in its own menu section, behind a confirmation that names the session and defaults to **Keep It Running**, so Enter cannot destroy anything. Sessions outliving tabs is the point of using a multiplexer, so nothing is cleaned up automatically. A New Tab does leave an auto-named session behind when its tab closes; those are marked **"no clients"** in the picker, and `mutiny ctl sessions --unattached-only` lists them so you can prune deliberately with `end-session`. Your persistent detached sessions appear there too, which is exactly why it is a view and not a cleanup.

**Right-click a tab** for Handoff Session to Haven, Rename Session, Copy Session Name, Close Tab (`Close Tab (session keeps running)` when multiplexer-backed, plain `Close Tab` on a plain shell), Move to New Window, and End Session. "Move to New Window" is deliberately not called "Detach": to a tmux user, detaching means detaching a client. Rename runs the multiplexer's own command (`tmux rename-session`, `zellij action rename-session`) and updates every Mutiny pane attached to that session, so labels, window titles, and the saved layout all follow. Handoff, Rename, Copy Name, and the `(session keeps running)` qualifier appear only on multiplexer-attached tabs and panes, never on plain shells. The menu never kills a multiplexer session: closing an attached tab kills only its client.

**Open Session** is a submenu listing discovered sessions inline, not a pop-up. It is filled at startup and re-listed every `refresh_interval_s` seconds, so it is populated before you open it and reflects sessions that came and went. A refresh keeps the sessions it already knows until each host's new result replaces them, so the list never flashes empty. `Ctrl+Shift+S` still opens the full picker, which is also the last item in that submenu.

Labels follow the Haven convention: **title is the session name, subtitle is the host** (this machine's hostname for local sessions, the SSH host for remote ones). Tabs and the picker both use two lines, title prominent and subtitle dimmer, with the title ellipsised so wrapped tabs stay readable. Session numbers are gone: once sessions are named, an index is redundant noise.

The toolbar is glyph-only with tooltips (`+ → ↓ ↗ ⌘ ?`), measured at 157 px against 505 px for the labelled version. The full verb names live in the menus.

## Standing policies (Tier 3)

A trusted agent can propose a scoped grant so it stops prompting for every action. **Proposing grants nothing.** The proposal waits in the Agent activity dialog (`Ctrl+Shift+J`) until you install it, and you can revoke it there at any time.

A policy names the verbs it covers (an empty scope is refused, never treated as "everything"), optionally pins scalar arguments to exact values, and carries a rate ceiling per rolling minute and an expiry. Beyond the ceiling, or outside the scope, calls fall back to the normal per-action prompt. Every covered call is still audited, as `approved-policy` rather than a prompt approval.

A policy **cannot escalate a carrier** and **cannot cover policy-management or Tier-4 irreversible verbs** (`end-session`, `quit`): it only substitutes for the prompt an `ask` carrier would raise on Tier-2 `Act` verbs. An `observe-only` peer stays read-only no matter what is installed, and a policy is bound to the kernel-verified peer that proposed it, so another process cannot use it.

```sh
mutiny ctl propose-policy --description "open tabs" --verbs spawn-tab,split \
  --max-calls-per-minute 30 --expires-in-minutes 60
mutiny ctl list-policies
mutiny ctl install-policy 1      # operator-side
mutiny ctl revoke-policy 1
```

The MCP surface mirrors Haven's vocabulary: `propose_policy`, `list_policies`, `install_policy`, `revoke_policy`. An agent cannot install its own proposal: `install_policy` and `revoke_policy` never ride on an `Allow Session` grant and cannot be included in a policy scope.

## Settings

`Ctrl+Shift+,` or Edit → Settings opens a dialog covering **every** key in the config store: font, all colours including the 16 ANSI entries, geometry, scrollback, `[sessions]`, `[peer]`, and `[notifications]`. Save writes `config.toml`, which live-reloads, so most changes apply without a restart. Reset to Defaults writes the defaults back.

A test counts the dialog's editable widgets against the number of config keys, so adding a key without exposing it fails the build rather than silently leaving it unreachable.

## App icon and launcher

Mutiny ships a Jolly Roger skull whose eye sockets are square terminal cells and whose mouth is a cursor bar. Flat white on deep navy (`#1A1A2E`), matching the Haven sibling icon's palette and weight, with no gradients or strokes so it stays legible at 16 px.

The SVG and seven PNG renders (16 to 512) for each of the two states live in `assets/icon/` and are embedded in the binary, so a plain `cargo run` shows the icon without installing anything: the assets are staged into `$XDG_CACHE_HOME/mutiny/icons` and added to the GTK icon theme search path.

To get a launcher entry and make the icon available to other apps:

```sh
mutiny install-desktop
```

That writes the hicolor icons and `~/.local/share/applications/dev.mutiny.Terminal.desktop`, pointing `Exec` at the binary you ran it from. It is idempotent: unchanged files are left alone so icon caches are not invalidated. Run it from your release build if that is what you launch.

**Agent-active state.** While an external agent is driving the terminal, the icon switches to a blue plate with the skull's eyes glowing red, so agent control is glanceable in a taskbar. It reverts to the idle skull when the agent goes quiet. The trigger is the same signal as the in-window agent badge (a non-Operator carrier call within the last 15 seconds), so the icon and the badge can never disagree, and that window is the debounce: a brief act holds the state rather than flickering. Blue and red are the GitHub Primer hues Haven uses. The installed `.desktop` keeps the idle icon, so the launcher never looks like an agent is running.

The application id, icon name, desktop basename, and `StartupWMClass` are all `dev.mutiny.Terminal`. The program name is set to that id too, because X11 `WM_CLASS` derives from the program name rather than the application id; without it the taskbar would show a second, iconless entry.

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
tab_strip_max_height_px = 96   # clamped 32..600; past this the tab strip scrolls

[terminal]
scrollback_lines = 10000       # capped at 1000000

[sessions]
hosts = []                     # remote hosts to scan; empty falls back to ~/.ssh/config Host aliases
tmux_program = "tmux"
zellij_program = "zellij"
ssh_program = "ssh"
# tmux_socket = "/tmp/private.sock"   # -S socket; set to keep experiments off your live sessions
connect_timeout_s = 5          # clamped 1..60
discovery_timeout_ms = 8000    # clamped 200..60000, then the pass reports partial
remote_term = "xterm-256color" # mutiny advertises TERM=alacritty, which remote hosts lack
create_if_missing = true       # attach-or-create
ignore_size = false            # true adds tmux -f ignore-size, so this client cannot resize other viewers
show_exited = true             # show zellij's resurrectable EXITED sessions, badged
refresh_interval_s = 15        # re-list sessions so the submenu stays current; 0 disables
preferred = "tmux"             # what New Tab uses: tmux, zellij, or none for a plain shell
root_session = "meta"          # constellation root session nesting sibling organ sessions; blank disables

[peer]
relay_unfocused_consent = true # auto-relay parked consent prompts to a connected Haven signer when unfocused
unfocused_grace_ms = 1500      # clamped 0..60000; delay before unfocused consent auto-relays
# default_haven_ssh_profile = "workstation"   # optional Haven SSH profile ID for handoff-to-peer / present-to-peer

[notifications]
long_command_secs = 20         # unfocused OSC 133;D command threshold in seconds; 0 disables
peer_relay_mode = "unfocused"  # unfocused, always, or off for Haven raise_notification mirroring
inbound_shade_allowlist = []   # empty = Haven-own events only; package substrings opt into list_notifications
```

Colors are `#rrggbb`. The 256-color cube and grayscale ramp beyond ANSI 16 are computed, not configurable.

**Live reload.** The file is stat'd every 250 ms and reloaded when it changes. Font, colors, palette, and padding apply immediately to every open pane: cell metrics are recomputed, which resizes the grid and the PTY. Geometry and scrollback depth apply to *newly created* panes and windows, because alacritty fixes scrollback at construction and resizing existing windows under you would be rude. Deleting the file reverts to defaults.

**Errors are survivable.** Unknown keys are rejected so a typo is reported rather than silently ignored. A malformed file leaves the previous config in place and warns, and does not retry until the file changes again. An invalid color falls back to that key's default; a palette that is not exactly 16 entries falls back to the whole default palette. Out-of-range numbers are clamped. None of these stop the terminal.

### Multiplexer sessions

`Ctrl+Shift+S` lists tmux and zellij sessions and opens the one you pick. Return opens it in a new tab, Shift+Return in a new window. Type to filter by session name, host, or multiplexer.

Mutiny does not reimplement multiplexing. The picker runs `tmux list-sessions` and `zellij list-sessions`, and attaching execs `tmux new-session -A -s NAME` (or `zellij attach --create NAME`) in the new pane. The multiplexer owns process survival: closing the pane kills only its client, never the server.

Sessions already attached elsewhere are **shared, not taken over**. Mutiny never passes `-d`, so opening a session that Haven is viewing on your phone leaves that client connected. Note that tmux's `window-size` defaults to `latest`, so the newest client's size wins and the other view may letterbox; set `ignore_size = true` to stop this client from resizing anyone.

Remote hosts come from `[sessions] hosts`, falling back to literal `Host` aliases in `~/.ssh/config`. Discovery uses `BatchMode=yes` and never prompts; attaching allocates a tty and lets you type a passphrase in the pane. A host that is unreachable or lacks the multiplexer is shown as an unavailable row with the reason, alongside the sessions that did answer.

One limit worth knowing: a pane running tmux gets no shell integration, because tmux is not a shell mutiny injects into. Prompt jumping and command capture do not work inside it unless tmux passes those sequences through.

### Session restore

On clean exit mutiny writes `$XDG_STATE_HOME/mutiny/session.json`, and on launch it restores that bundle unless you pass `--no-restore`. A restored session reproduces the window/tab/split layout with its divider ratios, each pane's working directory and grid size, and its scrollback.

`mutiny ctl save-session` snapshots on demand; `mutiny ctl quit` saves and exits. Closing is symmetric with opening: `close-pane`, `close-tab`, and `close-window`, each defaulting to the active target. Closing the last pane of a tab closes the tab, and the last tab closes its window.

Every IPC verb name also works as a `ctl` subcommand. Five verbs had a shorter CLI name than their wire name (`audit` vs `audit-log`, `run` vs `run-action`, `sessions` vs `list-sessions`, `screen` vs `screen-text`, `wait` vs `wait-prompt`); both spellings now work, so reading the IPC surface and typing it at the CLI no longer diverges.

The bundle carries the **opened session list with its window and tab ordering**, split trees and ratios, working directories, and scrollback. Reopening reproduces the arrangement exactly.

**Mutiny-owned shells are not restored.** There is no multiplexer daemon, so a crash kills them; that is a deliberate contract. A restore starts a fresh shell in each saved directory and replays the saved history into it. Restored scrollback is plain text, so colors and attributes are not preserved, and a crash loses changes since the last clean save.

**A pane attached to a multiplexer re-attaches.** Its session name and host are saved, and the next load attaches to that session again rather than starting a bare shell. This is not mutiny resurrecting processes: tmux or zellij owns them, and their survival was never mutiny's promise. Saved text is not replayed into an attaching pane, because the multiplexer owns and redraws that screen. If the session no longer exists, attach-or-create makes a fresh one under the same name, exactly as the picker does.

### Inline images

mutiny implements a vertical slice of the Kitty graphics protocol: transmit (including chunked), place, delete, and query, for PNG and raw RGB/RGBA. Images are anchored to cells and scroll with their content.

An agent can place an image through any of the three surfaces:

```sh
mutiny ctl present-media /path/to/chart.png --cols 40 --rows 12
```

or the `present_media` MCP tool with `{ "path": "...", "cols": 40, "rows": 12 }`, or the `present-media` IPC verb. It is an Act-tier verb, so it is consent-gated and audited like any other write. The reply carries `image_id` and `placement_id`.

Not implemented: animation frames, file and shared-memory transmission, unicode placeholders, and cursor-movement policies.

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
| Insert Haven TOTP code | `Ctrl+Shift+Y` |
| Search scrollback | `Ctrl+Shift+F` |
| Toggle menu bar | `Ctrl+Shift+M` |
| Command palette | `Ctrl+Shift+P` |
| Multiplexer session picker | `Ctrl+Shift+S` |
| Handoff mux session to Haven | `Ctrl+Shift+H` |
| Quick terminal (summon or dismiss) | ``Ctrl+Shift+` `` |
| Settings | `Ctrl+Shift+,` |
| Agent activity and audit log | `Ctrl+Shift+J` |
| Keybind cheatsheet | `Ctrl+Shift+?` |

The cheatsheet and the palette are generated from the same action registry as the menus and the IPC dispatch, so they cannot drift apart.

## Consent ladder

The tier is a property of the verb, not the caller. A human running `mutiny ctl` is the consent. An agent calling the same verb raises a prompt.

| Tier | Meaning | Status |
| --- | --- | --- |
| 1 | Observe. Read-only tools and resources, no prompt. | shipped |
| 2 | Act once. One-shot writes, per-action consent in window chrome. | shipped |
| 3 | Standing policy. A scoped, rate-capped, expiring grant installed once. | shipped |
| 4 | Irreversible. Always per-action, never standing (`end-session`, `quit`). | shipped |

Consent mode is assigned by the carrier that accepted the connection, never claimed by the client. An envelope carrying `consent_mode` is refused. Peer identity comes from the kernel via `SO_PEERCRED`, and a peer whose uid differs from the server's is refused outright.

Session grants bind to a kernel-verified peer identity (uid, pid, and process start time), not to the client label. The label is display and audit text and is not an input to the consent decision, so claiming another client's name cannot inherit its grant. A grant lives as long as the granted process: restarting an MCP bridge requires re-approval.

Every decision appends to a redacted, durable audit trail at `$XDG_STATE_HOME/mutiny/audit.jsonl`, recording the verb, its tier, the carrier, the peer identity, and the decision. Text payloads are logged as byte counts, never content. `Ctrl+Shift+J` opens the viewer, where session grants can be revoked.

Ask mode fails closed when neither a GUI window nor a connected signer-enrolled Haven limb is available to present a prompt. Consent chrome is not keyboard-reachable and no action verb resolves a consent, so input injection cannot confirm its own permission.

One limit is worth stating plainly: same-uid processes are mutually trusted under the Unix model. Any process running as you can reach the operator socket and act as the operator. The uid check closes the cross-user hole, not that one.

The tier follows what a verb actually does, not just its name. `list_sessions` with no host is Observe: it reads this machine. Naming a host makes the same verb Act, because it authenticates to that machine with your keys and runs a command there, and an observe-only agent must not get lateral movement for free. `open_session` is always Act.

The HTTP carrier adds a third path with a lower ceiling. It reaches the agent socket only, so the mode pinned by `--agent-consent` is the most a network peer can ever hold, and Operator is unreachable by construction. Pairing is a bearer token checked before the body is read.

## Development

```sh
cargo test                    # 241 tests, no display required
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
