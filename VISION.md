# Mutiny Product Vision

Status: draft v6, 2026-11-19. Owner: Ian. Public name remains open until first public release; internal identifiers are fixed now (see "Contracts that must not drift"). M0, the trust/parity half of M0.5, and the peer-identity grant binding are shipped; the growth diagnostic below says which is which. The next rung is chunked scrollback search (see M1).

## Thesis

Mutiny is a Linux desktop terminal written in pure Rust and pure `gtk4-rs`, built for KDE Plasma and GTK desktops without upstream code, Zig toolchains, or C-ABI baggage from Ghostty. It renders through GTK 4.22's GSK render-node pipeline (`GtkSnapshot`), so the GPU path is GTK's own NGL/Vulkan renderer rather than a per-pane GL context.

Mutiny is the workstation peer to Haven (`~/Code/Haven/VISION.md`). In Haven's vocabulary it is the desktop counterpart of two primitives: **Runtime**, a place to run things, and **Presence**, the surface in both directions. Haven's Namespace and Gateway reach Mutiny over SSH and WireGuard; Mutiny does not reimplement them. Humans and agents share the same observable terminal surfaces, act through the same verb vocabulary, and compose across local shells, tunnels, and MCP tool buses with the human keeping the wheel.

**The central design goal is that an agent is a second user of the same screen, not a caller of a separate API.** Everything below exists to serve that: the off-widget state, the single verb registry, the consent ladder, the audit trail.

## Identity

The strongest identity Mutiny has is: **the pure-Rust, KDE-native terminal that treats an agent as a first-class user of the same panes.** Every choice reinforces it. State lives outside widgets so a pane can move between windows without its shell noticing. One verb table feeds the menu bar, the palette, the CLI, and the MCP server, so nothing an agent can do is hidden from the human. Server-side decorations are left to KWin rather than overridden with a client-side headerbar. There is no C-ABI core, no libadwaita, no bundled model, and no vendor branding.

That combination is the position. A terminal that renders beautifully but exposes no addressable state is a screen an agent must be told about. A terminal that exposes state but keeps it inside widgets cannot move a pane without killing the process. Mutiny refuses both.

## The six commitments

1. **One committed VT engine, no facade.** `alacritty_terminal` is the only backend. No dual-backend trait abstraction and no foreign C library. Protocol extensions it does not carry (OSC 133 command zones, OSC 7 working directory, OSC 9;4 progress, output-quiescence turn tracking, and later Kitty graphics) live in a stream-level Rust `PaneEngine` wrapper. The wrapper slices the byte stream at OSC terminators so a marker observes the exact grid cursor position at the instant it arrives; a post-hoc event callback cannot give that. Sixel is out of scope.

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
6. **Local-first.** Plain TOML config with live reload, zero telemetry, zero outbound network calls. The config file is M1 work; until it lands, font and palette are constants and this principle is partly aspirational.

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
- **Network (M2, forward-looking).** `Paired` only after an explicit pairing token, bound to an explicitly configured interface. A network transport can never assign `Operator`.

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
- **Commitment 2, off-widget state: shipped and load-bearing.** Detach and move preserve scrollback and the running child, verified by test. The seam is split geometry: `SplitNode.ratio` is carried through tree surgery but never applied to the `GtkPaned`, and user drags are not written back.
- **Commitment 3, desktop citizenship: partly unverified.** SSD is left to KWin by construction and the chrome controls persist. The KDE global menu export has not been checked at runtime on a Plasma session; treat it as unproven until it is.
- **Commitment 4, presence: shipped for Tiers 1 and 2, trust boundary in place.** Snapshots, search, exited ring, notices, and the audit viewer work. Consent mode is carrier-assigned, peer uid is checked, session grants bind to kernel-verified peer identity, and the audit is durable. Remaining: Tiers 3 and 4 are not implemented, and same-uid processes remain mutually trusted on the operator carrier.
- **Commitment 5, one binary: shipped.** Three entry points, two carriers, MCP as adapter. The envelope carries a version and refuses a mismatch; MCP `initialize` negotiates against a supported revision set instead of echoing.
- **Commitment 6, parity: test-locked, not generated.** 21 IPC verbs, 17 MCP tools, 4 static resources. `list_actions` and `ui://mutiny/actions` close the slug-discovery hole; `read_screen_text` and `wait_prompt` close the two verbs that previously had no MCP counterpart. Tests assert both directions of the mapping. The four lists are still hand-maintained.

## Juxtapositions: surfaces that look unified and are not

Haven's audit found places where the surface looked unified while the implementations underneath were parallel, each taxing future work. Mutiny has three at M0:

- **The verb surface (largest instance, now locked rather than unified).** `IpcRequest`, `CtlVerb`, the MCP dispatch match, and `tool_definitions()` are still four hand-maintained lists describing one vocabulary, and a new verb still takes four edits. What changed in M0.5 is that drift now fails a test instead of shipping: `variant_ordinal` is an exhaustive match that will not compile without a decision for a new verb, `verb_samples` must cover it, and the parity tests assert reachability and dispatch in both directions. Generating the lists from one source remains the real fix.
- **The response shape.** `IpcResponse::Ok` is one struct of optional fields filled by destructuring, with constructors in two modules. A new field is a three-site edit. Worth collapsing before the verb count grows.
- **Configuration.** Principle 6 promises one TOML store; font, palette, and geometry are constants in the renderer. Until the store exists there is nothing to drift, but every new constant widens the eventual migration.

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

One honest gap remains. The CI workflow has not executed on GitHub because the repository has no remote; the commands it runs are verified green locally via `scripts/ci.sh`.

### M1: the surface becomes configurable and complete

**Next rung: chunk scrollback search across ticks.** The M0.5 baseline measured a full 10 000-line scan at about 3.2 ms median against a 4 ms GUI tick, so a non-matching needle run inline drops a frame. `search_scrollback` needs a resumable form that scans a bounded number of lines per tick and yields, and the viewport search bar must be built on that rather than on the whole-scan call. This comes first in M1 because it is a measured defect in the current shape, not a new feature.

- Resumable `search_scrollback` with a per-tick line budget, plus the viewport search bar over it.
- TOML config at `$XDG_CONFIG_HOME/mutiny/config.toml` with live reload, a documented key set, and defined precedence against CLI flags.
- Bundled shell integration snippets (OSC 133 + OSC 7) for bash, zsh, fish, and nushell. PTYs already export `TERM_PROGRAM=mutiny` and `MUTINY_PANE_ID` for this.
- Kitty graphics placements in `PaneEngine` plus `GtkSnapshot` texture nodes, and `present-media` for inline agent overlays.
- Drag-to-reorder and drag-to-detach over the existing keyboard mobility verbs; split ratios applied and persisted.
- OSC 777 notifications through the desktop portal, with tab badges.
- Text input correctness: `GtkIMContext` for IME and dead keys, and a font fallback chain for CJK, emoji, and box-drawing glyphs.
- Wire the existing damage API (`has_damaged_lines`, `reset_damage`) into the redraw path, or drop the claim from this document.

Done when: a search with no matches never stalls a tick, a fresh install reads config, a plain bash session emits OSC 133 without manual setup, an image renders inline, and a CJK or accented compose sequence types correctly.

### M2: persistence and reach *(forward-looking)*

- Session restore and named workspace bundles matching Haven's `save_workspace`/`compose_workspace`: window, tab, and split geometry, working directories, serialized scrollback, with no background multiplexer daemon.
- Quick terminal: Yakuake-style drop-down via `gtk4-layer-shell`, toggled over IPC/MCP or a global shortcut.
- Authenticated streamable-HTTP MCP listener, off by default, bound to an explicitly configured WireGuard or SSH-tunneled interface, with pairing tokens and rate-capped Tier 3 standing policies.

Done when: a restored bundle reproduces geometry and scrollback, and the HTTP listener refuses an unpaired client and can never assign `Operator`.

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
