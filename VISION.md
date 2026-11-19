# Mutiny Product Vision

Status: draft v19, 2026-11-19. Owner: Ian. Public name remains open; internal identifiers are fixed (see "Contracts that must not drift"). The repo is public at `GlassOnTin/mutiny` under AGPL-3.0-or-later. M0, the trust/parity half of M0.5, peer-identity grant binding, chunked scrollback search, bundled shell integration, TOML config, terminal capability queries, IME with font fallback, the Kitty graphics slice, split ratios with drag gestures, OSC 777 notifications, and damage tracking are shipped. **M1 is complete.** M2 is open: session restore, the authenticated HTTP MCP listener, per-peer identity for that listener, and a tmux/zellij session picker are shipped; quick terminal, Tier 3 standing policies, and a consent mode per network peer remain.

## Thesis

Mutiny is a Linux desktop terminal written in pure Rust and pure `gtk4-rs`, built for KDE Plasma and GTK desktops without upstream code, Zig toolchains, or C-ABI baggage from Ghostty. It renders through GTK 4.22's GSK render-node pipeline (`GtkSnapshot`), so the GPU path is GTK's own NGL/Vulkan renderer rather than a per-pane GL context.

Mutiny is the workstation peer to Haven (`~/Code/Haven/VISION.md`). In Haven's vocabulary it is the desktop counterpart of two primitives: **Runtime**, a place to run things, and **Presence**, the surface in both directions. Haven's Namespace and Gateway reach Mutiny over SSH and WireGuard; Mutiny does not reimplement them. Humans and agents share the same observable terminal surfaces, act through the same verb vocabulary, and compose across local shells, tunnels, and MCP tool buses with the human keeping the wheel.

**The central design goal is that an agent is a second user of the same screen, not a caller of a separate API.** Everything below exists to serve that: the off-widget state, the single verb registry, the consent ladder, the audit trail.

## Identity

The strongest identity Mutiny has is: **the pure-Rust, KDE-native terminal that treats an agent as a first-class user of the same panes.** Every choice reinforces it. State lives outside widgets so a pane can move between windows without its shell noticing. One verb table feeds the menu bar, the palette, the CLI, and the MCP server, so nothing an agent can do is hidden from the human. Server-side decorations are left to KWin rather than overridden with a client-side headerbar. There is no C-ABI core, no libadwaita, no bundled model, and no vendor branding.

That combination is the position. A terminal that renders beautifully but exposes no addressable state is a screen an agent must be told about. A terminal that exposes state but keeps it inside widgets cannot move a pane without killing the process. Mutiny refuses both.

## The six commitments

1. **One committed VT engine, no facade.** `alacritty_terminal` is the only backend. No dual-backend trait abstraction and no foreign C library. Protocol extensions it does not carry (OSC 133 command zones, OSC 7 working directory, OSC 9;4 progress, output-quiescence turn tracking, and Kitty graphics) live in a stream-level Rust `PaneEngine` wrapper. The wrapper slices the byte stream at OSC terminators so a marker observes the exact grid cursor position at the instant it arrives; a post-hoc event callback cannot give that. Sixel is out of scope.

2. **State outside widgets, reparent-safe rendering.** Terminal state, PTYs, and scrollback live in an application-owned `Workspace` keyed by `PaneId`, never inside GTK widgets. Widgets are disposable viewports over `GtkSnapshot` render nodes rather than per-pane `GLArea` contexts, so detaching or moving a split across windows never tears down rendering state or stalls a PTY. Anything that must survive a reparent belongs in the engine or the workspace.

3. **First-class KDE and GTK citizenship.** `gtk4-rs` without libadwaita. Server-side decorations (`xdg-decoration` on KWin) are respected natively so Breeze titlebars and borders work out of the box. A toggleable horizontal `PopoverMenuBar` (`Ctrl+Shift+M`), `GMenuModel` export for the KDE global menu, and persistent tab-bar controls stay visible in both CSD and SSD modes.

4. **Shared human-and-agent presence.** Every surface obeys Haven's three presence laws (observable state, actionable API, reachable surface), detailed under "Presence" below. Consent and audit are part of the law, not a feature bolted onto it; see "Consent and trust".

5. **One binary, three entry points, vendor-neutral by rule.** `mutiny` serves the GUI, `mutiny ctl` for humans and scripts, and `mutiny mcp` as a stdio JSON-RPC 2.0 MCP server tunnelable from Haven over SSH or WireGuard. The MCP server is a protocol adapter over the same Unix socket IPC, never a second implementation. Mutiny ships no LLM, no chat pane, no API keys, and no vendor-specific agent branding.

6. **Parity by construction, proven by test.** One declarative `ActionSpec` table generates the keybind router, `PopoverMenuBar`, right-click menu, command palette, cheatsheet, and the `run_action` dispatch shared by CLI and MCP. The broader IPC verb surface is still four hand-maintained lists rather than one generated table, but it is now test-locked: an exhaustive match forces a reachability decision for every new verb, and tests assert that every verb reaches an advertised MCP tool or resource and that every advertised tool actually dispatches. Parity is a checked property, not yet a generated one.

## Primary user

A Linux desktop power user on Wayland or X11, KDE Plasma or a GTK desktop, running concurrent local shells, remote sessions, and AI agents, including cross-device workflows driven from or alongside Haven. macOS and Windows are out of scope.

## Principles

1. **Linux desktop is the flagship.** Wayland-first with X11 support. KDE SSD and GTK CSD both behave like native citizens.
2. **1:1 verb parity.** Menu bar, palette, cheatsheet, right-click menu, `mutiny ctl`, and `mutiny mcp` read from one registry, and a test asserts it.
3. **No silent automation, no divergent state.** Agent reads and writes operate on the exact `Workspace` rendered on screen, leave an audit trail, and surface visibly in window chrome.
4. **Corporate and vendor neutrality.** Any CLI or MCP client speaking standard PTY and JSON-RPC conventions is a first-class peer. Vendor names never appear in product chrome or user-facing documentation. Haven is named in this document as the reference peer, not as a vendor or an endorsement; the rule binds the shipped app and its public docs, not internal design notes.
5. **Pure Rust, human-understandable code.** Dependencies come from crates.io and standard Linux system libraries (`gtk4`, `pango`, `harfbuzz`, `fontconfig`). Every merged change must be explainable unaided by a human maintainer, which means a change too large to explain is a change to split.
6. **Local-first.** Plain TOML config with live reload, zero telemetry, zero outbound network calls. Shipped: `$XDG_CONFIG_HOME/mutiny/config.toml`, with CLI flags taking precedence over the file over built-in defaults.

## Presence: the Haven peer contract

Haven's Presence primitive runs in two directions: humans and agents reach in to operate the substrate, and the substrate reaches back out to the human's attention. Mutiny owns the desktop end of that contract. Three laws apply to every surface it builds.

**Observable state.** Structured pane snapshots (cursor, modes, OSC metadata, semantic command output), scrollback search, and a ring of the last eight exited panes are exposed both over IPC and as addressable `ui://mutiny/*` MCP resources. The exited ring exists because a fast-failing session is otherwise undiagnosable: the pane is gone before anyone reads it. If a human can see it, an agent can query it.

**Actionable API.** `send-input` separates UTF-8 text, bracketed-paste framing, and raw named keys (`enter`, `esc`, `ctrl-c`, arrows) so a raw-mode agent REPL never mangles a multi-line turn. `send-to-agent` and `await-turn` support both OSC 133 shell prompts and output-quiescence settle windows, because most agent TUIs emit no shell integration at all. There is no agent-only bypass layer: every verb an agent calls is a verb the human can watch happen.

**Reachable surface.** An agent can post an inline operator notice (`present-notice`) that lights up `urgent-next` (`Ctrl+Shift+A`), so the channel runs back to the human's attention rather than around it. Every call carries a verb-level consent tier, surfaces on a live Agent activity pill, and lands in an inspectable audit log (`Ctrl+Shift+J`). Consent chrome is never keyboard-reachable and no action verb resolves a consent, so input injection cannot confirm its own permission.

### Build vs delegate

The same rule Haven uses, pointed at the desktop: **build the surface where it must compose with panes and agents inside Mutiny; delegate to the desktop where the handoff is clean.**

Mutiny builds its own terminal rendering, pane mobility, and agent surface because no desktop primitive offers them and because they must compose with each other in one process. It delegates window decoration to KWin, notifications to the desktop portal, font discovery to fontconfig, and session multiplexing to `tmux` and `zellij`, which compose alongside `mutiny ctl` rather than being reimplemented. The test is the same one Haven uses: if the user would want to invoke a second Mutiny verb while looking at this view, Mutiny owns the view.

## Consent and trust: the tier belongs to the verb, trust belongs to the carrier

Mutiny adopts Haven's consent ladder unchanged, so a peer moving between the two projects carries one vocabulary:

- **Tier 1 (observe).** Read-only tools and resources. No prompt.
- **Tier 2 (act once).** One-shot writes. Per-action consent in window chrome, or a scoped session grant.
- **Tier 3 (standing policy).** A declarative grant the agent proposes and the operator installs once, against a declared scope (which verbs, optionally pinned arguments), a rate ceiling, and an expiry. Revocable from the Agent activity screen; every covered call still audited; beyond the ceiling, calls fall back to per-action prompts. *(Forward-looking, M2.)*
- **Tier 4 (irreversible).** Anything that destroys state the operator cannot recover. Always per-action, never standing. *(Forward-looking; the verb set is not yet enumerated.)*

The tier is a property of the **verb, not the caller**. A human running `mutiny ctl` *is* the consent. An agent calling the same verb raises the prompt. Reflexes never escalate themselves.

### The carrier assigns identity; the client never claims it

Shipped in M0.5. `consent_mode` is no longer a field a client can send: an envelope carrying it is refused before dispatch, on both carriers. Identity comes from the kernel via `SO_PEERCRED`, checked before anything the client wrote is trusted, and a peer whose uid differs from the server's is refused outright.

Two carriers, because one socket cannot serve both trust paths:

- **Operator socket** (`$XDG_RUNTIME_DIR/mutiny.sock`). A same-uid peer is assigned `Operator`. `mutiny ctl` arrives here. The human running it *is* the consent.
- **Agent socket** (`mutiny-agent.sock` beside it). Every connection is assigned the mode the operator pinned at GUI launch with `--agent-consent` (default `ask`). `mutiny mcp` arrives here by default. A bridge's own `--consent` flag is a local ceiling it enforces before forwarding; it can be more restrictive than the carrier, never less. A bridge asking for `paired` against an `observe-only` carrier is denied and audited.
- **Network (M2, shipped as a bounded slice).** Authenticated streamable-HTTP MCP, off unless `--http-bind` names an explicit interface; a wildcard bind is refused. Every request must carry the pairing token, checked before the body is read. It forwards to the agent socket only, so the mode pinned by `--agent-consent` is the most a network peer can hold and `Operator` is unreachable by construction rather than by a check.

The network carrier inherits the agent carrier's ceiling. HTTP has no kernel peer credential, so the pairing token is the credential: each named peer has its own token, its own relay carrier, and its own identity, and the trail records that identity rather than the relaying process. Session grants key on it, so an Allow Session tap for one peer does not cover another. The client still cannot claim an identity; it presents a token and the operator's configuration fixes what that token maps to.

One gap remains on this path: the consent mode is one setting for the whole listener, so per-peer trust levels (one peer paired, another observe-only) are not expressible yet. Requests are stateless in this slice, so the name sent in `initialize` does not persist; calls audit under the peer's configured label.

Session grants key on a kernel-verified `PeerIdentity` (uid, pid, and `/proc` start time), never on the `client` label. The label is display and audit text; it is not an input to the consent decision at all, so there is no value it could take that would make a grant apply. Start time is included so a recycled pid does not inherit a dead process's grant. A grant therefore lives as long as the granted process: restarting an MCP bridge requires re-approval, which is the intended trade. The audit records the peer alongside the label, so the trail shows which process actually acted.

One limit stays visible rather than implied: same-uid processes are mutually trusted under the Unix model. A malicious process running as the user can reach the operator socket and be the operator. The uid check closes the cross-user hole (notably the `/tmp` fallback path), not the same-user one. What the identity binding does close is grant inheritance between distinct same-uid processes, which is the hole a spoofed label opened.

### Audit is evidence, so it has to outlive the process

Shipped in M0.5. Every decision appends to `$XDG_STATE_HOME/mutiny/audit.jsonl` (falling back to `~/.local/state`), redacted as before: text payloads are recorded as byte counts, never content. Each entry now also records the carrier it arrived on, so the trail shows which trust path produced a decision. The file rotates past 8 MiB keeping one previous generation. A fresh controller reloads the tail into the 256-entry viewer ring and continues the sequence rather than restarting it, which is covered by a restart test.

## Architectural direction

Three layers, each small and sharp:

1. **Engine and topology.** `PaneEngine` wraps exactly one VT backend and owns the PTY; `Workspace` owns pane identity and the split tree. Neither knows GTK exists.
2. **Controller.** `AppController` owns the tick, the consent and wait queues, the audit trail, and all dispatch. It is the only place that knows both the workspace and the window.
3. **Surfaces.** `ui` renders and captures input; `ipc` and `mcp` are transports over the same verbs; `action` is the registry the surfaces read from.

```text
mutiny (single Rust binary: GTK4 GUI + `mutiny ctl` CLI + `mutiny mcp` server)
├── action      Single ActionSpec registry + ConsentTier -> keybinds, GMenuModel, palette, cheatsheet
├── engine      PaneEngine: alacritty_terminal 0.26 + PTY + OSC 133/7/9;4 + quiescence + named keys
├── topology    Widget-free Workspace: PaneId arena + SplitNode + urgent_next + 8-slot ExitedPane ring
├── app         AppController: 4ms tick, IPC dispatch, consent/wait queues, audit ring, window lifecycle
├── ipc         JSONL over $XDG_RUNTIME_DIR/mutiny.sock + consent modes + redacted AuditEntry ring
├── mcp         Stdio JSON-RPC 2.0 MCP adapter (`tools/*` + `ui://mutiny/*`) over ipc, no engine access
└── ui          Pure gtk4-rs (v4_22): MutinyWindow (SSD chrome, consent/notice bars, audit view) + TerminalView
```

One thread, one 4 ms tick, no async runtime. Deferred replies are queues holding a cloned socket, not futures: a handler returns nothing and the stream is parked until the tick satisfies it. Vocabulary is fixed: the pane store is the `Workspace`, not a separate "PaneArena" type, so a grep for either name finds the same thing.

## Growth diagnostic: where each commitment sits today

A plan needs a diagnosis. Against the M0 bootstrap commit:

- **Commitment 1, engine: exercising well.** Chunked OSC 133 capture across read boundaries and scrollback rotation, OSC 7 cwd, OSC 9;4 progress, quiescence, and named-key encoding are implemented and tested. Two claims are ahead of the code. "Line-level damage tracking" exists as `has_damaged_lines`/`reset_damage` with no callers, so rendering is full-frame. The Kitty encoder honours disambiguation and report-all-keys, not all five flags.
- **Commitment 2, off-widget state: shipped and load-bearing.** Detach and move preserve scrollback and the running child, verified by test. Split geometry is closed: `SplitNode.ratio` is applied to the `GtkPaned`, written back on drag, and carried through tree surgery and session restore. The GTK gesture wiring itself is still not covered by tests; the model operations it calls are.
- **Commitment 3, desktop citizenship: partly unverified.** SSD is left to KWin by construction and the chrome controls persist. The KDE global menu export has not been checked at runtime on a Plasma session; treat it as unproven until it is.
- **Commitment 4, presence: shipped for Tiers 1 and 2, trust boundary in place.** Snapshots, search, exited ring, notices, and the audit viewer work. Consent mode is carrier-assigned, peer uid is checked, session grants bind to kernel-verified peer identity, and the audit is durable. A third path now exists: the authenticated HTTP carrier reaches the agent socket only, so a network peer can never hold `Operator`. HTTP peers now carry their own token-derived identity, so grants and the trail are per peer. The session picker composes with external multiplexers, and its remote listing is Act because it spends the operator's credentials. Remaining: Tiers 3 and 4 are not implemented, same-uid processes remain mutually trusted on the operator carrier, and the network consent mode is still one setting for the whole listener rather than per peer.
- **Commitment 5, one binary: shipped.** Three entry points, two Unix carriers plus an optional HTTP carrier, MCP as adapter. The envelope carries a version and refuses a mismatch; MCP `initialize` negotiates against a supported revision set instead of echoing.
- **Commitment 6, parity: test-locked, not generated.** 22 IPC verbs, 18 MCP tools, 4 static resources plus a per-pane snapshot resource. `list_actions` and `ui://mutiny/actions` close the slug-discovery hole; `read_screen_text` and `wait_prompt` close the two verbs that previously had no MCP counterpart. Tests assert both directions of the mapping. The four lists are still hand-maintained.

## Juxtapositions: surfaces that look unified and are not

Haven's audit found places where the surface looked unified while the implementations underneath were parallel, each taxing future work. Mutiny has three at M0:

- **The verb surface (largest instance, now locked rather than unified).** `IpcRequest`, `CtlVerb`, the MCP dispatch match, and `tool_definitions()` are still four hand-maintained lists describing one vocabulary, and a new verb still takes four edits. What changed in M0.5 is that drift now fails a test instead of shipping: `variant_ordinal` is an exhaustive match that will not compile without a decision for a new verb, `verb_samples` must cover it, and the parity tests assert reachability and dispatch in both directions. Generating the lists from one source remains the real fix.
- **The response shape.** `IpcResponse::Ok` is one struct of optional fields filled by destructuring, with constructors in two modules. A new field is a three-site edit. Worth collapsing before the verb count grows.
- **Configuration (resolved).** Principle 6's single TOML store now exists and the renderer reads font, palette, geometry, and padding from it. The remaining discipline is to keep new tunables going into the store rather than becoming fresh constants beside it.

## Milestones

Ordered by leverage, not effort. Each carries the criteria that make it done.

### M0: self-hosting rebel *(shipped, bootstrap commit)*

Pure `gtk4-rs` windowing with KWin SSD, toggleable menu bar, palette, cheatsheet, audit viewer. `PaneEngine` over `alacritty_terminal` 0.26 with PTY lifecycle, OSC 8 hyperlinks, OSC 52 clipboard, OSC 7, OSC 9;4, OSC 133 A/B/C/D journal, and quiescence tracking. Linux selection model: primary on drag, middle-click paste, distinct `Ctrl+Shift+C`/`V`, sanitized bracketed paste. Binary split trees and tabs: `split-right`, `split-down`, `detach-split`, `move-pane`, `urgent-next`, eight-slot exited ring. Unix socket IPC, `mutiny ctl`, `mutiny mcp` with `--consent ask|paired|observe-only`.

Verified at the bootstrap commit: 13 tests passed, with and without a display. Detach preserves scrollback and captured command output; chunked OSC 133 yields the right exit code; the exited ring retains fast-failing panes; observe-only MCP is refused for Act verbs while paired is allowed, and both are audited. The suite has since grown to 28 under M0.5.

### M0.5: trust, parity, and verification *(shipped)*

The work that makes M0 safe to build on. Nothing here was a feature.

- Carrier-assigned identity per "Consent and trust": two carriers, `SO_PEERCRED` uid check, and refusal of any envelope carrying `consent_mode`.
- Durable redacted audit at `$XDG_STATE_HOME/mutiny/audit.jsonl`, with rotation, carrier recorded per entry, and reload on startup.
- `list_actions` tool and `ui://mutiny/actions` resource, plus `read_screen_text` and `wait_prompt` so no IPC verb is CLI-only.
- Parity locks: `variant_ordinal` exhaustive match, `verb_samples` coverage test, verb-to-MCP reachability test, advertised-tool dispatch test.
- IPC envelope version with refusal on mismatch; MCP `initialize` negotiates against a supported revision set.
- CI workflow (`.github/workflows/ci.yml`) and `scripts/ci.sh` running fmt, clippy with `-D warnings`, the displayless suite, and the same suite under Xvfb so the GTK accelerator assertions actually execute.
- Latency and throughput baseline recorded in `benches/BASELINE.md`, reproduced by `cargo bench`.

Evidence: the suite passes displayless and under a display. The spoof test refuses `operator`, `paired`, and `ask` claims on both carriers while the same verb without the claim succeeds. The carrier test shows a `paired` bridge denied against an `observe-only` carrier, audited with that carrier, then succeeding once the operator re-pins. The restart test shows two entries surviving a controller rebuild and the sequence continuing.

Follow-on rung, shipped: session grants bind to `PeerIdentity` rather than the client label. The proof runs two real child processes under the same uid, both claiming `trusted-peer`; the granted one is allowed on a fresh connection and the other fails closed, with the audit showing two distinct pids behind one label. Mutating the decision back to "any grant applies" makes both the policy test and the cross-process test fail, so the tests detect the vulnerability rather than merely passing alongside the fix.

One honest gap remains. The release workflow is tag-triggered and gated on CI, but no tag has been pushed, so the artifact packaging and publication path is unexercised. CI itself runs green on GitHub at `GlassOnTin/mutiny`.

### M1: the surface becomes configurable and complete

**Shipped rung: chunked scrollback search.** `ScrollbackSearch` is a resumable bottom-up cursor advanced by `SEARCH_LINES_PER_TICK` (512 lines). Both the viewport search bar (`Ctrl+Shift+F`) and the IPC `search` verb use it, sharing one tick budget across however many scans are in flight. The IPC request and response shapes are unchanged; only the latency differs, with the reply parked across ticks like `wait` and `await-turn`. A scan restarts if output shifts the grid mid-flight, so collected matches never point at lines that moved.

Evidence: across three runs the maximum chunk was 176 to 233 us, 4.4% to 5.8% of the 4 ms tick, and a full-history miss spreads over 20 ticks. Total wall time for the chunked miss overlaps the whole-scan range, so chunking caps per-tick cost without measurably raising total cost. `one_chunk_cannot_scan_a_full_history` asserts the budget deterministically rather than by timing.

**Shipped rung: bundled shell integration.** Snippets for bash, zsh, fish, and nushell are compiled into the binary with `include_str!` and materialized under `$XDG_RUNTIME_DIR/mutiny/shell-integration/` when a pane spawns. Each shell is pointed at a wrapper that loads the user's own config first, then the snippet, so no dotfile is edited: bash gets `--rcfile`, zsh gets a `ZDOTDIR` wrapper set that preserves the original, fish gets `-C source`, and nu gets a generated `--config` wrapper. Injection is best-effort: an unwritable runtime dir costs the markers, never the pane.

Evidence: `tests/shell_integration.rs` spawns a fresh bash with an empty HOME and no shell override, and asserts OSC 133;A on the first prompt, captured output and exit 0 for a successful command, exit 1 for `false`, OSC 7 cwd, and that no dotfile was created. zsh, fish, and nushell were driven in real PTYs in an Arch container and all emit A/B/C/D with correct exit codes. Two shell-specific facts shaped the snippets: fish 4 emits OSC 133 natively, so on 4.x the snippet contributes only OSC 7 to avoid duplicating every marker; and nu has no `char escape`, so ESC is `char -i 27`.

Discovered defect, since fixed: fish 4 blocks at startup until the terminal answers its capability queries. `PaneEngine` now answers XTVERSION, OSC 10/11, and DCS terminfo, and alacritty's existing answers for primary DA, DSR, and the Kitty query are left alone. See the capability-query rung below.

**Shipped rung: TOML config.** `$XDG_CONFIG_HOME/mutiny/config.toml` with a documented key set covering font, colors and the 16-entry palette, geometry, and scrollback depth. Precedence is CLI flag, then file, then built-in default, and overrides are re-applied on every reload so editing the file cannot discard a launch flag. The file is stat'd every 250 ms; font, colors, palette, and padding apply immediately to open panes (cell metrics are recomputed, which resizes the grid and PTY), while geometry and scrollback apply to new panes and windows because alacritty fixes scrollback at construction. Unknown keys are rejected, a malformed file keeps the previous config, bad colors fall back per key, and out-of-range numbers clamp. Nothing here can take the terminal down.

Evidence: `tests/config_reload.rs` covers fresh-install defaults, picking up a newly created file, live font and palette edits, flags surviving a reload while unflagged keys follow the file, malformed-edit recovery, deletion reverting to defaults, and configured geometry and scrollback reaching new panes. `src/config.rs` unit tests cover parsing, precedence, clamping, path resolution, and mtime detection.

**Shipped rung: terminal capability queries.** `PaneEngine` now answers the queries alacritty leaves unanswered: XTVERSION (`DCS >| mutiny <version> ST`), OSC 10/11 color queries (reporting the configured foreground and background, scaled to xterm's 16 bits per channel), and DCS terminfo. Primary DA, secondary DA, DSR cursor position and status, and the Kitty keyboard query were already answered by alacritty and are deliberately not answered twice; a test asserts each gets exactly one reply.

Terminfo answers only what mutiny can truthfully claim. `query-os-name` returns `linux` because mutiny is Linux-only; everything else is declined with `DCS 0 + r ST`, which is a valid answer that unblocks the caller. Silence is what blocks it.

Evidence: `tests/terminal_queries.rs` asserts each reply's exact bytes, that queries split across reads are still answered, that ordinary output produces no replies, and that a real fish 4 starts, receives answers, and executes a command. The fish assertion counts replies rather than only checking that fish ran, because some fish versions time out their queries and proceed anyway; disabling the responses makes the test fail with zero replies. CI installs fish so the assertion runs against Arch's fish 4.9 rather than skipping.

**Shipped rung: IME and font fallback.** Each `TerminalView` now owns a `GtkIMMulticontext` wired to its key controller via `set_im_context`, so the controller filters keys through the system IM module (ibus, fcitx) or GTK's simple context before the raw key encoder sees them. Committed text is written to the PTY as UTF-8, preedit is drawn underlined at the cursor and cleared on commit, and the IME is told the cursor location so its candidate window lands nearby. Font fallback is Pango's, resolved through fontconfig; the rung verified it rather than reimplementing it.

Evidence: `tests/ime_and_fallback.rs` drives the real commit signal with CJK, accented, and emoji text and asserts each appears in the pane, that preedit clears, that CJK/emoji/box-drawing/accented samples produce zero unknown glyphs, and that a mixed Latin+CJK string resolves to two different fonts (DejaVu Sans Mono and Noto Sans Mono CJK JP on the test host). GTK needs one thread, so all of it lives in a single test function; it skips without a display and CI runs it under Xvfb with CJK and emoji fonts installed.

The rung also found and fixed a real bug: an unrealized view has a 0x0 allocation, and `recompute_grid_dimensions` was resizing its pane to the minimum 4x2 grid, which wrapped every wide glyph and ate characters. It now waits for a real allocation, with a regression assertion.

**Shipped rung: Kitty graphics vertical slice.** `src/graphics.rs` holds images and cell-anchored placements as pure data with no GTK dependency, so `PaneEngine` owns the store and the UI decodes bytes into textures. The APC scanner recognizes `ESC _ G ... ST` and supports transmit (chunked), transmit-and-place, place, delete, and query, in PNG or raw RGB/RGBA, with z-index ordering and sanitized OK/error replies that honor `q`. The renderer draws placements as `GtkSnapshot` texture nodes, scrolled with the content they are anchored to, with a texture cache invalidated by a store generation counter.

`present-media` is the desktop peer of Haven's `present_media`: an Act-tier verb that reads an image file and places it inline in a pane, reachable from `mutiny ctl present-media`, the `present_media` MCP tool, and IPC. It is additive: existing request and response shapes are unchanged, the envelope stays version 1, and two optional response fields (`image_id`, `placement_id`) were added. The surface is now 22 verbs, 18 tools, 4 resources, still held at parity by test.

Evidence: `tests/kitty_graphics.rs` asserts protocol placement and the OK reply, chunked reassembly with `q=1` suppression, sanitized error replies, the present-media verb end to end including a clean missing-file error, and that a presented image produces a texture node in the widget's render-node tree while an empty pane produces none. Both PNG and raw RGBA render. Removing the `append_texture` call makes the render test fail, so it discriminates.

Scope deliberately excluded: animation frames, file and shared-memory transmission, unicode placeholders, and cursor-movement policies. Placements use the same absolute line coordinate as OSC 133 markers, which is stable while history grows but drifts once scrollback is capped; that limitation is shared with prompt markers and documented rather than hidden.

**Carried follow-up, now done:** CI asserts `fish --version` so the fish startup test cannot silently skip if the package install fails.

**Shipped rung: split ratios and drag gestures.** `SplitNode.ratio` is now applied to the `GtkPaned` and user drags are written back to the tree, so a divider position survives widget rebuilds, tab switches, detach, and move. Ratios clamp to 0.05..0.95 so a pane can never be dragged out of existence. Application waits for a real allocation via a tick callback, because `map` fires before size allocation and applying there silently left the default 50/50.

Tab buttons carry a `DragSource` with the tab id and a `DropTarget` for reorder; the content area carries a `DropTarget` that detaches the dragged tab's active pane through the existing `detach_split` verb. Drag therefore invokes the same model operations as the keyboard verbs, so the two cannot diverge.

Evidence: `tests/split_ratios.rs` asserts under Xvfb that a split stored at 0.30 renders at 0.300, that moving the divider writes 0.620 back to the tree, and that a rebuild reapplies the persisted ratio. Headless tests cover path-targeted ratio writes, clamping, persistence across detach/move/further splits, tab reorder with index clamping, and the controller operations the drag handlers call.

Persistence is within the workspace model, which is what survives rebuilds and reparenting. Cross-restart persistence arrives with session restore in M2; there is no config or state file for geometry yet.

The GTK gesture wiring itself is not driven by tests: synthesizing a real drag needs a seat and a compositor. The model operations it invokes are tested, and the ratio write-back is tested by moving the divider programmatically, which is what the handle does.

**Shipped rung: OSC 777 notifications.** `PaneEngine` parses `777;notify;<title>[;<body>]` with either BEL or ST termination and across chunked reads, raises `AttentionReason::OscNotification`, and emits a `DesktopNotification` event. The engine stays GTK-free; `src/notify.rs` forwards to `org.freedesktop.Notifications.Notify` on the session bus. That is the interface desktops implement; there is no separate notification portal method, so calling it directly is the correct path rather than a shortcut.

The notifying pane's tab gets a `!notify` badge through the same attention mechanism as bell and agent notices, and focusing the pane clears it. Sending is best-effort: a missing session bus logs and drops, never taking down a pane.

Evidence: `tests/osc777.rs` covers title and body parsing, title-only, ST termination, chunked arrival, and rejection of empty titles and non-notify subcommands. A controller test emits the sequence through a real PTY and asserts the injected recorder receives exactly `("mutiny", "Deploy finished", "all green")`, and that attention is set then cleared on focus. Under Xvfb, a window test asserts the tab label gains `!notify`. Removing the forward call makes the controller test fail.

The D-Bus send itself is not exercised by tests: CI's container has no session bus. Forwarding is verified through the injected notifier, and the no-bus path is verified to return an error rather than panic.

**Shipped rung: damage tracking, which closes M1.** GtkSnapshot is immediate-mode, so there is no partial framebuffer update to be had: every `snapshot()` call rebuilds a render-node tree for the whole widget. The implementation therefore caches one render node per row and rebuilds only the rows alacritty reports damaged, re-appending cached nodes for the rest. `PaneEngine::take_damage` returns viewport row indices and resets the accumulator.

A cache key covering grid size, cell metrics, scroll offset, config, graphics generation, preedit, and selection forces a full rebuild when any of those change, because they move or restyle content that line damage does not track. A blank row appends nothing and an empty snapshot yields no node, so each row gets a transparent rect to guarantee a cacheable node; without it every empty line rebuilt every frame.

Measured under Xvfb on a 43-row pane of styled text: an idle frame rebuilds 1 row (alacritty re-marks the cursor line on every `damage()` call) at 4.9 us, against 646.9 us for a full repaint, roughly 131x cheaper. A one-line change rebuilds 2 rows at 36.5 us, roughly 17.7x cheaper. Scroll and font changes rebuild every row, which is correct. `tests/damage_tracking.rs` asserts all of this, and ignoring damage makes it fail.

The rung also fixed a real pre-existing bug it exposed: `apply_config_to_views` held a workspace borrow across a `for` loop and then borrowed mutably inside it, which panics on any config reload with a pane open. No earlier test reloaded config with a live pane.

Render measurement covers node construction and Pango shaping under Xvfb, not GPU compositing or presentation, which need a real compositor.


M1 is complete. Every item above is shipped and tested; the render path has a measured baseline in `benches/BASELINE.md`.

### M2: persistence and reach

**Shipped rung: session restore.** `src/session.rs` captures the workspace to `$XDG_STATE_HOME/mutiny/session.json`: window/tab/split trees with their ratios (reusing the live `WindowModel`/`SplitNode` types so geometry is carried verbatim), each pane's title, working directory, grid size, and scrollback as text. It writes atomically, rejects an unknown version rather than guessing, and restores on launch unless `--no-restore` is passed. Saving happens on clean exit.

Processes are not restored, and that is the contract rather than a gap: there is no multiplexer daemon, so a crash kills every shell. A restore spawns a fresh shell in each saved directory and replays the saved scrollback into it, so the layout, history, and split ratios come back while the processes do not. Restored scrollback is plain text, not styled cells, because alacritty's grid is not a stable serialization format. A crash loses changes since the last clean save.

Evidence: `tests/session_restore.rs` builds a three-pane session with distinct cwds and history, saves it, restores into a fresh controller, and asserts window/tab/pane identity, tree shape and both split ratios, per-pane grid size, cwd, and scrollback content. It then writes a command to a restored pane and asserts a fresh shell executes it, making the no-daemon contract visible. A second test asserts a missing file restores nothing and a corrupt file is reported without half-restoring. Disabling scrollback replay makes the first test fail.

**Shipped rung: the authenticated HTTP MCP listener.** `src/http_mcp.rs` serves `POST /mcp` on an explicitly named interface, off unless `--http-bind` is passed. A wildcard bind is refused. Every request needs `Authorization: Bearer <token>`, compared before the body is read; the token is supplied with `--http-token` or generated from the kernel CSPRNG and printed once. It authenticates on its own threads and forwards to the agent Unix socket through the same `McpBridge::handle_line` stdio uses, so it is a carrier rather than a second implementation.

A network client can never be assigned `Operator`. The config carries an agent socket path and nothing else, so no code path reaches the operator socket, and the effective mode is whatever `--agent-consent` pinned at launch. This is the composition joint for Haven driving the desktop terminal from a phone.

Evidence: `tests/http_mcp_carrier.rs` asserts that no token and a wrong token both get 401 while the right token is served, that a `consent_mode: operator` field in the JSON-RPC body changes nothing and the audit records the carrier as the pinned agent mode, that a refused request adds no audited verb, and that an `observe-only` carrier denies an Act verb over HTTP. Both security assertions were mutation-checked: wiring the carrier to the operator socket fails two tests with `"carrier":"operator"` in the audit, and removing the pairing gate fails two tests with a 200 where 401 was required. Four unit tests cover wildcard refusal, token comparison, token generation, and spawn validation. The real binary was also driven under Xvfb: 401 without a token, 401 with a wrong one, 200 with the right one, `initialize` negotiating 2025-06-18, `list_workspace` returning the live tree, `GET` refused with 404, and an Act verb under the default `ask` carrier timing out on the consent prompt rather than acting.

Limits of the slice, stated rather than implied: no SSE streaming, no MCP session management, no `DELETE`. Requests are stateless, so `initialize`'s client name does not persist and every call audits as `http-peer`. The consent mode is one setting for the whole listener, so per-peer trust levels are not expressible yet; identity and grants are per peer. It has now been driven end to end from Haven on a phone through an SSH local forward: unpaired and wrong-token requests got 401, the paired token got 200, all 18 tools listed, a command sent from the phone executed on the desktop and its output was read back, `spawn_pane` and `present_notice` were accepted, and with the carrier pinned to `observe-only` both Act verbs were denied and the denied command never ran. The audit showed `paired` then `observe-only` with `denied` decisions and never `operator`, and every call mapped to the mutiny process's own pid, which is the shared-identity gap observed rather than inferred. A second run bound the listener to the workstation's own WireGuard address on a gateway-hosted WG server and drove the same verbs from the phone across the tunnel, so both named transports are now exercised rather than one.

**Shipped rung: per-peer identity for the HTTP carrier.** HTTP has no `SO_PEERCRED`, so the pairing token is the peer credential. `--http-peer LABEL=TOKEN` is repeatable; each peer gets its own relay Unix carrier, and `AppController::start_http_peer_listener` fixes a `PeerIdentity::for_token` on that carrier. The HTTP layer authenticates the token by constant-time comparison against every configured token, then dials that peer's carrier. Identity is therefore assigned by a carrier, as on the Unix path, and the remote client cannot select it: it can only present a token, and the token-to-carrier mapping is fixed at launch. `PeerIdentity` gained an optional `token_peer` fingerprint (stable FNV-1a, deliberately not cryptographic, since it is only ever computed from an already-accepted token), with `pid` 0 so a token peer can neither collide with nor inherit a process's grant. Because `consent_outcome` already keys grants on `PeerIdentity`, per-peer grants came with per-peer identity at no extra cost.

Evidence: `two_peer_tokens_produce_two_distinct_audit_identities` drives two tokens through the real HTTP listener and asserts the durable trail records two different `token_peer` fingerprints, each equal to the fingerprint of the token presented, stable within a peer, with `pid` 0 and never `carrier=operator`, and that a token outside the set is still refused and reaches no verb. `token_peer_grants_are_per_peer_and_never_cross_to_a_process` asserts at the policy function that a grant for one peer leaves another at `Park`, and that a process peer does not inherit a token peer's grant. Three mutations were checked: collapsing all fingerprints to one value fails both tests, routing both peers to one relay socket fails the identity test, and accepting any token as the first peer fails three tests including unpaired refusal. A live run from Haven on a phone showed two labels with two fingerprints in the audit, both peers driving the same pane, an unknown token refused, and the same fingerprint recurring across a restart, which is what makes the trail comparable over time.

Design for the remaining half, recorded rather than ground on: a consent mode per peer means storing a `ConsentMode` on each `HttpPeerCarrier` instead of reading the shared `agent_mode` at accept time, plus a CLI syntax for it (for example `LABEL=TOKEN:MODE`). The identity, audit, and grant plumbing above already supports it; only the mode assignment and its CLI are missing. That is a small bounded rung, not an architectural change.

**Shipped rung: the multiplexer session picker.** `Ctrl+Shift+S` lists tmux and zellij sessions, local and over SSH, and opens the chosen one in a new tab or window. This is the handoff the delegation promise implied but never built: Mutiny said it composes alongside tmux and zellij, yet getting a session into a pane meant typing `tmux attach` by hand with no way to see what existed. By the build-vs-delegate test above, the picker is a view Mutiny owns (you want a second verb while looking at it: open in tab, or in window) while the multiplexing stays delegated. No daemon is added, so the crash-semantics contract is untouched: closing a pane kills the tmux client, never the server.

It attaches with `tmux new-session -A -s NAME`, which shares a session rather than taking it over, so opening something Haven is viewing on a phone leaves that client connected. Discovery is polled on the tick with `try_wait` rather than threaded, because alacritty mutates the process environment on every pane spawn. The consent tier follows what the verb does: listing this machine is Observe, naming a remote host is Act, because that spends the operator's SSH credentials on another machine.

Evidence: 21 unit tests in `src/mux.rs` cover the parsers, both quoting layers, host validation, and the argv shapes without needing any multiplexer installed; `tests/mux_sessions.rs` drives real tmux on private sockets for discovery, shared attach (two clients stay connected), a session literally named `probe;`, the tick budget against a hanging multiplexer, deadline kill-and-reap, and remote quoting through a fake ssh shim that reproduces the remote login shell; `tests/session_picker.rs` proves the picker survives a topology rebuild with its selection and dies with its window. Seven mutations were checked and each failed a named test: dropping the `--` host guard, dropping `-t`, dropping the semicolon escape, reverting the delimiter to `|`, switching zellij to `--short`, making remote listing unconditionally Observe, and replacing `try_wait` with a blocking wait. The picker test found and fixed a real leak, where a programmatically closed window left its picker state alive.

Limits: only tmux and zellij are discovered; multiplexer panes get no shell integration, so prompt jumping does not work inside them; and session restore does not re-attach, so a restored tmux pane returns as a plain shell. That last one edits a stated contract and is left for its own rung rather than changed silently.

**Next rung: quick terminal.** A Yakuake-style drop-down via `gtk4-layer-shell`, toggled over IPC/MCP or a global shortcut.

Still open in M2: rate-capped Tier 3 standing policies, and a consent mode per network peer.

Done when: the quick terminal toggles from a global shortcut. Session restore and the HTTP listener's refusal and never-Operator conditions are already met.

## Contracts that must not drift

These are cheap to state now and expensive to retrofit.

- **Wire stability.** The IPC envelope and MCP tool names are a contract Haven and other peers depend on. The envelope now carries version 1 and refuses a mismatch, and MCP `initialize` negotiates rather than echoes. What remains is discipline: bump the version on a breaking shape change, and deprecate verbs rather than renaming them. Resource URIs follow Haven's `ui://<product>/<surface>` shape.
- **Performance budget.** A terminal is judged on input-to-glyph latency and on throughput under a full-screen redraw. `benches/latency.rs` records the engine baseline in `benches/BASELINE.md`; re-run it before and after any change to the ingest or render path. No target is set yet, and the render path is still unmeasured because it needs a display. Full-frame repaint plus a Pango layout per styled run per frame remains the likely cost centre.
- **Remote working directories.** The thesis names SSH, but OSC 7 parsing discards the host and keeps the path, and new panes inherit that path as a local directory. Splitting inside an SSH session therefore tries to spawn a local shell in a remote path. Track the host with the cwd and refuse to inherit across a remote boundary.
- **Naming.** The public name is open; the identifiers are not. `dev.mutiny.Terminal`, `MUTINY_SOCKET`, `MUTINY_PANE_ID`, `TERM_PROGRAM=mutiny`, the socket filename, and `ui://mutiny/*` are wire contract and stay stable regardless of what the product is eventually called.
- **Crash semantics.** There is no multiplexer daemon, so a GUI crash kills every shell. This is accepted, and it is the reason session restore is M2 rather than optional. State it plainly so nobody arrives from tmux expecting survival.
- **Accessibility.** The `Accessible` interface is declared on the terminal widget and not implemented. Either scope a11y in explicitly or record it as out of scope; silence is the one option that ages badly.

## Scope boundaries: what Mutiny is not

A tight scope is how a small project stays coherent. Mutiny deliberately does not:

- **Reimplement tmux or zellij.** No background multiplexer daemon. They compose alongside `mutiny ctl` and `mutiny mcp`.
- **Ship an AI assistant.** No bundled model, chat client, prompt language, or default endpoint. The user brings the agent.
- **Name or bundle any vendor's agent CLI.** No vendor onboarding, branding, or endorsement in the app or its public docs. Protocol names are configuration options, not endorsements.
- **Depend on libadwaita or force client-side decorations.** No GNOME-only chrome assumptions.
- **Carry Ghostty code, Zig build steps, or C-ABI core bindings.** One pure-Rust VT engine, no facade.
- **Support Sixel.** Kitty graphics is the image path.
- **Target macOS or Windows, ship telemetry, or keep a second GUI config store.** One platform family, one config file, no outbound calls.

---

The engine is sufficient and the state model is right. The work now is trust, parity, and proof: making the consent ladder a boundary rather than a convention, making the verb vocabulary one list rather than four, and making each milestone carry the test that says it is done.
