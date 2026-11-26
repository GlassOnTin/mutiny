# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Linux desktop terminal in pure Rust + `gtk4-rs` (no libadwaita, no Ghostty/Zig/C-ABI code), designed as the workstation peer to Haven: humans and agents act on the same panes through the same verb vocabulary. One binary plays three roles: GTK GUI, `mutiny ctl` CLI, and `mutiny mcp` stdio MCP server.

`VISION.md` is the product and architecture spec (draft v25). M0, the trust/parity half of M0.5, peer-identity grant binding, chunked scrollback search, bundled shell integration, TOML config, terminal capability queries, IME with font fallback, the Kitty graphics slice, split ratios with drag gestures, OSC 777 notifications, damage tracking, per-cell glyph layout caching, host-checked OSC 7 working directories, and Tier 3/4 plus `PolicyChange` consent enforcement are shipped. M1 is complete. M2 is shipped save for an unscheduled system-wide global shortcut: session restore, the authenticated HTTP MCP listener with per-peer identity and per-peer consent mode, the tmux/zellij session picker, the summonable scratch quick terminal, and the Settings dialog are shipped. M3 is complete (`0.2.0`): M3.1 bilateral peer pairing (`peers.json`, symmetric `initialize` tap-to-pair, stdlib `HavenPeerClient`, `list-peers` / `pair-haven` / `revoke-peer`, and the chrome Limb Status Pill), M3.2 mirrored authentication and HMAC-signed cross-limb consent (`enroll-haven-signer`, unfocused `PendingConsent` co-signing across Tiers 2-4 with `EgressGrantSigner` HMAC-SHA256 verification, inline Haven Auth Bar, and Operator-only `insert-peer-totp`), M3.3 focus-aware bidirectional notification and `OSC 133;D` long-command attention mirror (`[notifications]`, `CompositeNotifier` with per-peer `TokenBucket` rate limiting, zero-`EVERY_CALL` default `haven_only` inbound alerts, opt-in `inbound_shade_allowlist` polling, and `list-peer-alerts` / `clear-peer-alerts`), and M3.4 unified constellation and paired-Haven session tree (`session-tree`, `open-session-grid`, `handoff-to-peer` `Ctrl+Shift+H`, and `present-to-peer`) are shipped. Read `VISION.md` for intent and constraints, not as a description of current behaviour. Its "Growth diagnostic" section is the authoritative list of which claims are ahead of the code.

Doc style: VISION.md follows the structure of Haven's `~/Code/Haven/VISION.md` (thesis, identity, numbered commitments, presence laws, growth diagnostic, juxtapositions, milestones, scope boundaries). Match that structure, but use plain punctuation. No em-dashes, including in section headings; use a colon or a full stop instead.

## Commands

```bash
cargo build                              # requires GTK 4.22+ dev libs (pkg-config gtk4)
cargo test                               # 241 tests: 125 unit + 7 config + 12 e2e + 7 http + 17 m3 + 10 mux + 5 menuia + 8 manage + 4 mouse + 3 quick + 1 sessmenu + 1 restoregui + 1 settings + 4 policy + 1 tabact + 4 smoke + 1 picker + 1 tabstrip + 1 appicon + 1 agenticon + 1 cursor + 1 damage + 1 ime + 5 graphics + 5 osc777 + 2 ratios + 4 session + 1 shell + 7 queries
cargo test --test e2e_ipc_pty            # integration only (live PTYs, carriers, audit, MCP)
cargo test --test http_mcp_carrier       # HTTP carrier: pairing refusal, never-Operator, observe-only
cargo test --test mux_sessions           # session discovery/attach (private tmux sockets; skips without tmux)
cargo test --test session_picker         # picker lifetime under rebuild (needs a display)
cargo test detach_split                  # single test, by name substring
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo bench                              # headless engine baseline, see benches/BASELINE.md
./scripts/ci.sh                          # everything CI runs, forced displayless
```

Edition 2024, declared `rust-version = 1.92`. Clippy (with `-D warnings`) and fmt are clean.

**Check against the CI toolchain, not just your local one.** CI runs Arch's current stable, which is newer than most local installs and carries clippy lints yours lacks. A rung once passed locally and failed CI on `chunks_exact_to_as_chunks`. Install the CI version (`rustup toolchain install <ver> --profile minimal --component clippy rustfmt`) and run `cargo +<ver> clippy --locked --all-targets -- -D warnings` before pushing.

**Tests need no display.** `env -u WAYLAND_DISPLAY -u DISPLAY cargo test` passes. The integration tests drive real `/bin/sh` PTYs through `AppController` with no GTK application attached: `ensure_ui_window` returns early when `gtk_app` is `None`, so no widgets are created. Engine, topology, IPC, and MCP work is therefore testable headlessly; only `src/ui/` needs a display.

One caveat: `action::tests::every_action_id_is_registered_uniquely` calls `gtk4::init()` and validates accelerator syntax only if that succeeds. Headless runs silently skip the accelerator check.

Manual smoke test of the GUI plus its control surfaces:

```bash
MUTINY_SOCKET=/tmp/mutiny-dev.sock cargo run -- --shell /bin/sh --agent-consent ask
./target/debug/mutiny ctl --socket /tmp/mutiny-dev.sock list
./target/debug/mutiny ctl --socket /tmp/mutiny-dev.sock split --down
# mcp defaults to the agent carrier beside the operator socket
MUTINY_SOCKET=/tmp/mutiny-dev.sock ./target/debug/mutiny mcp --consent observe-only
```

Operator socket resolution: `--socket` (global clap arg, works before or after the subcommand) > `$MUTINY_SOCKET` > `$XDG_RUNTIME_DIR/mutiny.sock` > `/tmp/mutiny-<uid>.sock`. The agent socket is `agent_socket_path()` of that: `mutiny.sock` becomes `mutiny-agent.sock`. `mutiny ctl` uses the operator socket; `mutiny mcp` uses the agent socket unless `--socket` is given explicitly. Launching a second GUI against a live operator socket forwards `spawn-window` to the first instance and exits.

## Architecture

### MCP is a protocol adapter, not a second implementation

`src/mcp.rs` never touches `PaneEngine` or `Workspace`. `McpBridge` translates JSON-RPC methods into `IpcRequest` values and sends them over the same Unix socket as `mutiny ctl`, tagging each with `CallerMeta { client, consent_mode }`. All behaviour lives in `AppController::execute_ipc_request` (`src/app.rs`). When an agent verb misbehaves, debug the IPC handler, not the MCP layer.

Consequence: the verb surface is hand-maintained in parallel across four files, now held together by tests rather than by a generated table. Adding one IPC verb means:

1. `src/ipc.rs`: `IpcRequest` variant, arms in `consent_tier()`, `verb_name()`, `variant_ordinal()`, and `redacted_summary()` if it carries a payload, plus an entry in `verb_samples()`.
2. `src/app.rs`: arm in `execute_ipc_request()`.
3. `src/mcp.rs`: arm in `mcp_surface_for()`, arm in `call_tool()`, and an entry in `tool_definitions()`.
4. `src/main.rs`: `CtlVerb` variant and its mapping in `run_ctl()`.
5. `tests/e2e_ipc_pty.rs`: probe arguments in `every_advertised_mcp_tool_dispatches`.

Three of those are enforced rather than remembered. `variant_ordinal` and `mcp_surface_for` are exhaustive matches, so a new variant will not compile until they are updated. `verb_samples_cover_every_variant_exactly_once` then fails on the ordinal bounds check until a sample exists, and the two parity tests fail if the verb reaches no advertised tool or resource, or if an advertised tool does not dispatch. Expect a compile error first and a test failure second; both are the lock working.

`IpcResponse::Ok` is a single struct of all-optional fields that handlers fill by destructuring `&mut resp`. A new response field also needs updating in `IpcResponse::empty_ok()` and `ids_ok()` in `app.rs`.

The `ActionSpec` registry is separate and *is* generated: `ACTIONS` in `src/action.rs` drives GTK accels, `win.<slug>` actions, the menu bar, context menu, palette, cheatsheet, `ctl actions`, and the `run_action` MCP tool. A new GUI verb needs an `ActionId` variant, an entry in both `ActionId::ALL` and `ACTIONS`, and an arm in `AppController::execute_action`. A test asserts `ALL` and `ACTIONS` stay the same length with unique slugs and chords.

### State lives outside widgets

`Workspace` (`src/topology.rs`) owns `BTreeMap<PaneId, PaneEngine>` plus `Vec<WindowModel>` → `TabModel` → binary `SplitNode` tree. `PaneEngine` holds the PTY and the `alacritty_terminal` `Term`. GTK widgets are disposable viewports: `MutinyWindow::rebuild_from_model` tears down every `TerminalView` for the active tab and rebuilds them from the tree on each topology change. Only the active tab has widgets.

This is what makes `detach-split` and `move-pane` work without restarting shells: tree surgery relinks `PaneId`s and never moves an engine. Anything that must survive a reparent belongs in `PaneEngine` or `Workspace`, never in a widget.

### One thread, one 4 ms tick, no async runtime

Everything is `Rc`/`RefCell` on the GTK main loop. `main.rs` installs `glib::timeout_add_local(4ms)` calling `AppController::poll_once`, which drains every PTY, queues redraws, satisfies pending waits, expires consents, closes exited panes, then accepts IPC clients.

Deferred work is modelled as queues holding a cloned `UnixStream`, not futures: `PendingWait` (for `wait`/`await-turn`) and `PendingConsent` (for Tier-2 `ask`). Returning `None` from `execute_ipc_request` or `enqueue_wait` means "no reply yet, the stream is parked"; `Some(resp)` is written immediately. `poll_once` runs on the GUI thread, so handlers must not block. Note that `accept_ipc_clients` reads the request envelope inline with a 2 s read timeout, so a slow client does stall the tick.

`RefCell` discipline matters: `TerminalView::render_snapshot` holds `workspace.borrow()` for the whole draw, and `poll_once` scopes its borrows narrowly. A `borrow_mut` reached during a draw panics.

### OSC extensions are a stream-level side channel

`PaneEngine::advance_bytes` runs its own byte-wise OSC scanner and **slices the byte stream at each OSC terminator**, feeding the preceding slice to alacritty's `Processor` before handling the payload itself. This is deliberate: OSC 133 markers must observe the exact grid cursor position at the instant they arrive, which a post-hoc event callback cannot give. New protocol extensions (OSC 133/7/9;4 today, Kitty graphics later) go in `handle_osc_payload`, not in a widget.

Alacritty's own events (title, bell, OSC 52 clipboard, `PtyWrite` replies) arrive separately via `EventCollector` and are drained in `collect_term_events`. Two paths, both surfaced as `EngineNotification` to `poll_once`.

### Capability queries: answer only what alacritty does not

alacritty already answers primary DA, secondary DA, DSR (cursor position and status), and the Kitty keyboard query. `PaneEngine` answers the rest: XTVERSION, OSC 10/11 color queries, and DCS terminfo. **Do not add replies for the ones alacritty handles.** A double reply confuses the querying program, and `alacritty_handled_queries_are_answered_exactly_once` asserts against it.

Two mechanisms, matching the two sequence families. OSC 10/11 ride the existing OSC side channel (`handle_osc_payload` arms). XTVERSION (`CSI > q`) and DCS terminfo (`DCS + q ... ST`) use a separate `QueryScanState` scanner with its own cross-chunk state, so it does not disturb the OSC slicing that OSC 133 cursor accuracy depends on. Replies queue into `pending_pty_writes` and flush in `drain_pty`, the same path alacritty's `Event::PtyWrite` uses.

OSC 10/11 report `reported_bg` / `reported_fg`, which the controller sets from config at spawn and on reload via `set_reported_colors`, so the answer matches what is rendered. Terminfo answers only `query-os-name` (linux, a fact) and declines everything else with `DCS 0 + r ST`. A decline is a valid answer that unblocks the caller; silence is what blocks it. Do not claim capabilities the engine has not been shown to implement.

`query_replies_sent()` counts replies. The fish startup test asserts it is non-zero, which is what makes the test discriminate: some fish versions time out their queries and proceed anyway, so "fish ran a command" alone passes even with responses disabled.

### PTY teardown is bounded

alacritty's `Pty::drop` sends SIGHUP and then blocks in `child.wait()`. A shell that does not exit on SIGHUP hangs teardown forever. This surfaced as a CI hang in tests that create and drop several PTYs, and it would hang app shutdown the same way. `PaneEngine` now implements `Drop`: SIGHUP, poll `waitpid(WNOHANG)` for up to 100 ms, then SIGKILL and reap. Reaping here makes alacritty's later `wait()` return ECHILD instead of blocking.

Do not remove that Drop or replace it with a plain SIGHUP. Exit-code capture is unaffected because `drain_pty` observes `ChildExited` before the engine is dropped.

### The binary smoke test is the guard for the real startup path

`tests/binary_smoke.rs` spawns the actual binary (`env!("CARGO_BIN_EXE_mutiny")`), drives it through `ctl`, and checks launch, spawn, close, save, quit, and relaunch-with-restore. Every other GUI test constructs widgets directly, which bypasses startup ordering entirely: session restore was fully covered headlessly and still broken in the GUI. This test reproduces that bug when both halves of its fix are reverted, which is the point of having it.

It skips when `DISPLAY` is empty (the displayless CI job) and uses a fake tmux fixture, so it is deterministic and never touches real sessions. A `Guard` kills the child on drop, and readiness is the control socket answering `ctl list`, not a sleep.

### CLI naming: both spellings work

Five IPC verbs had shorter ctl names than their wire names. They now carry `#[command(alias = ...)]` so the wire name also resolves, without breaking the short name. `every_ipc_verb_name_is_accepted_by_the_cli` walks every verb name and asserts `ctl <name> --help` succeeds, so a new verb cannot drift again.

### Session restore

**`attach_gtk_app` must materialise any model window that has no view.** `ensure_ui_window` returns early when no application is attached, so a restore that runs before startup populates the model but creates no widgets, leaving an application with nothing to show, which GTK then exits. This was a real bug: session restore had never worked in the GUI, and the headless tests could not see it because they never attach an application. Restore now runs in `connect_activate`, and `attach_gtk_app` also creates views for existing model windows so the ordering is safe either way.

**The exit save must not run on an empty workspace.** `handle_window_closed` removes the window from the model before quitting, so the save after `app.run` captured nothing and wiped the bundle. Closing the last window is how people actually quit, so that path lost the layout every time. `save_before_last_window_goes` captures it while the last window still exists, and the exit save is skipped when the workspace is empty. `tests/restore_gui.rs` restores BEFORE attaching on purpose to guard it.



`src/session.rs` captures geometry by reusing the live `WindowModel`/`SplitNode` types, so ratios and tree shape serialize verbatim rather than being re-derived. Pane content is title, cwd, grid size, and scrollback as plain text. `Workspace::restore_from_session` replaces the model with saved ids and counters; `AppController::restore_session` spawns the engines first, then installs the tree that references them.

Mutiny-owned shells are never restored. That is the no-daemon contract, not a missing feature: a restore spawns fresh shells in saved cwds and replays scrollback. Do not add process resurrection for them without revisiting the crash-semantics contract in VISION.

Five verbs fill gaps the CLI surface had: `save-session` (snapshot now, not only on clean exit), `close-pane`, `close-tab`, `close-window` (the counterparts to the spawn verbs, previously reachable only through `run close-pane`), and `quit` (save then quit, deferring the quit to an idle callback so the reply is written first). All are Act. `close_tab` closes the tab's panes so the existing cascade applies rather than duplicating teardown logic.

`SessionPane.mux` is the exception and not a violation: a pane attached to tmux or zellij saves its `MuxSessionRef` and RE-ATTACHES on restore, because the multiplexer owns those processes. Two rules follow. Do not replay saved scrollback into an attaching pane, since the multiplexer owns and redraws that screen. And a remote session's saved cwd is a path on that host, so it must not become a local cwd. The field is `#[serde(default)]`, so bundles written before it still load and `SESSION_VERSION` stays 1.

Save is atomic (temp file plus rename) and happens on clean exit in `main.rs`, not in `shutdown_ipc`, so tests do not write to the user's real state directory. Tests must call `set_session_path` with a temp path. Load rejects an unknown version and reports corrupt files rather than half-restoring.

`replay_scrollback` feeds plain text through `advance_bytes`, so it fabricates no command records or prompt markers. Text containing a literal ESC could be reinterpreted on replay; that is a known edge of text-only restore.

### The HTTP carrier is a third path with a lower ceiling

`src/http_mcp.rs` is a carrier, not a second MCP implementation: it authenticates, then hands the body to `McpBridge::handle_line`, the same JSON-RPC handler stdio uses. It runs on its own threads (one per connection, 5 s IO timeouts) because it must not touch the `Rc`/`RefCell` GUI state; it reaches the controller only through the agent Unix socket.

The never-Operator guarantee is structural. `HttpMcpConfig` carries an `agent_socket` and nothing else, so there is no code path from this module to the operator socket. The effective mode is whatever `--agent-consent` pinned at launch. Do not add an operator path here, and do not let a JSON-RPC body field influence the mode.

Pairing is checked before the body is read, so an unpaired client cannot make the listener allocate or forward. `validate_bind` refuses a wildcard address. Token comparison is length-independent over equal-length slices.

Per-peer identity works by giving each token its own relay carrier. `start_http_peer_listener` binds one Unix socket per configured peer and fixes a `PeerIdentity::for_token` on it; `accept_http_peer_carriers` accepts on each with that identity as an override, still after the same-uid kernel check. The HTTP carrier authenticates the token, then dials that peer's socket. So the identity is assigned by a carrier, exactly as on the Unix path, and nothing the remote client writes can select it. `PeerIdentity.token_peer` carries a stable FNV-1a fingerprint of the token; `pid` stays 0 so a token peer can never collide with or inherit a process's grant. Because `consent_outcome` already keys grants on `PeerIdentity`, per-peer grants came free with per-peer identity.

Do not collapse these back into one socket: that is precisely the shared-identity bug, and `two_peer_tokens_produce_two_distinct_audit_identities` fails if you do.

Each `HttpPeerCarrier` also pins its own `ConsentMode`, so `accept_http_peer_carriers` uses `peer.mode` rather than the shared `agent_mode`. That is what lets one peer be `paired` while another is `observe-only`, and it is why a test pins the agent carrier to `observe-only` while giving one peer `paired`: an implementation still reading `agent_mode` fails it.

Never-Operator now has two independent gates. `ConsentMode::parse_peer_mode` cannot produce `Operator` at all, so no configuration string reaches it, and `start_http_peer_listener` refuses `Operator` defensively in case a future caller bypasses the parser. Both are tested, and both were mutation-checked.

`--http-peer LABEL=TOKEN[:MODE]` is parsed by `http_mcp::parse_http_peer`. The suffix is only stripped when it names a real mode, so a token containing a colon still works; `:operator` is refused rather than absorbed into the token, because it signals a mistaken expectation. Without a suffix the peer inherits `--agent-consent`.

One limit remains: requests are stateless (no MCP session id), so `initialize`'s client name does not persist; calls audit under the peer's configured label.

### Multiplexer sessions: pure argv core, tick-polled discovery

`src/mux.rs` is pure data and command construction, like `graphics.rs`: parsers, argv builders, and quoting, with no GTK and no subprocess execution. That is what makes the security rules unit-testable without tmux, zellij, or ssh installed. The controller owns the processes.

Three hazards are load-bearing, each verified against the real programs:

- The tmux listing format separates fields with a **literal TAB**, never `|`. A `|` is legal in a session name (`tmux has-session -t 'a|b'` reports it intact), so a pipe delimiter silently corrupts the parse. `splitn(5, '\t')` keeps `session_path` whole.
- tmux re-parses a **trailing `;` in its own argv**, with no shell involved. `tmux_escape_name` inserts one backslash before a final `;` only. Escaping interior semicolons is a bug: tmux preserves the backslash there.
- ssh joins its command arguments into a string the **remote login shell** parses. `shell_quote_posix` is applied to every element including the `-F` format string, and the program name is part of that string. The host is preceded by `--` and validated, because a leading-dash host would otherwise be read as an ssh option (`-oProxyCommand=…` is local code execution).

Discovery and attach use different ssh shapes on purpose: discovery is `-n -o BatchMode=yes` with no tty so it can never prompt; attach is `-t -e none` so the user can type a passphrase and a typed `~.` cannot kill the session. Remote attach prefixes `TERM=xterm-256color` because mutiny advertises `TERM=alacritty`.

**Discovery is polled, not threaded.** `PendingDiscovery` holds a `std::process::Child`; `advance_discoveries` runs in `poll_once` between `advance_searches` and `expire_pending_consents`, draining pipes with `O_NONBLOCK` under `DISCOVERY_BYTES_PER_TICK` and reaping with `try_wait`. Threads are deliberately avoided: alacritty's `tty::setup_env` calls `unsafe env::set_var` on every pane spawn, so a worker thread reading `environ` concurrently would be unsound. Drain incrementally, never wait-then-read: a child that fills the pipe buffer would block forever and `try_wait` would never fire. On deadline, `kill` then keep reaping, because `Child::drop` does neither.

`begin_discovery` returns `Option<IpcResponse>` with the usual parking convention. One reply slot: a second requester during a pass gets the current snapshot. `cache_gen` bumps on every change and the picker refreshes only when it moves, never every tick.

**Consent follows what the verb does.** `ListSessions { host: None }` is Observe (this machine only); naming a host is Act, because it spends the user's SSH credentials on another machine. The GUI picker scans configured remotes through `discovery_targets_all`, which IPC never calls: the human is the consent there. Do not make `host: None` mean "local plus configured remotes", or an observe-only carrier gets lateral movement for free.

`spawn_engine` takes a per-pane `command` override; `create_*_with_command` are the entry points and the plain `create_*` delegate with `None`. There are four `spawn_engine` callers including `restore_session`. A pane's multiplexer label is pinned via `set_mux_label`, because `Event::ResetTitle` would otherwise leave the tab reading "shell".

The picker is a separate modal top-level cloned from `show_command_palette`, so `rebuild_from_model` cannot tear it down. It is stored on the window and cleared on **every** close path (`close_request` and `close_programmatically`), not left to GTK's transient-destroy propagation, which does not fire for a programmatic close. Build rows and drop the index borrow before `append`/`select_row`: the selection handler reads that same index. Selection is restored by identity, not row index.

Tests must use a **private tmux socket** (`-S` in `[sessions]`) and `kill-server` on teardown. `window-size` is `latest`, so attaching to a developer's live session resizes every other viewer, including a phone.

### Damage-driven redraw

GtkSnapshot is immediate-mode, so "repaint only the damaged region" means caching a render node per row and rebuilding only damaged rows, not partial framebuffer updates. `render_snapshot` keeps `row_cache` plus a `RenderCacheKey`; `PaneEngine::take_damage` returns viewport row indices (alacritty reports them viewport-relative already) and resets the accumulator.

The cache key must cover everything that moves or restyle content without line damage: grid size, cell metrics, scroll offset, config, graphics generation, preedit, selection. Missing one shows up as stale rendering, not as a crash.

Two traps. A blank row appends nothing and `Snapshot::to_node()` returns None for an empty snapshot, so every row needs a transparent rect to be cacheable. And alacritty re-marks the cursor line on every `damage()` call, so an idle frame rebuilds one row, not zero; tests assert `<= 1`, not `== 0`.

`rows_rebuilt_last_frame()` and `invalidate_render_cache()` exist for tests and the render benchmark. The benchmark needs a display: run it with `GDK_BACKEND=x11 xvfb-run -a cargo bench`.

### OSC 777 and M3.3 bidirectional notifications

The engine parses `777;notify;<title>[;<body>]` and emits `EngineNotification::DesktopNotification`, keeping GTK out of the engine. `src/notify.rs` wraps `DesktopNotifier` (`org.freedesktop.Notifications.Notify` on the session bus) inside `CompositeNotifier`, which always delivers locally and conditionally mirrors to connected outbound Haven limbs via `raise_notification` according to `[notifications] peer_relay_mode` (`unfocused` | `always` | `off`), gated per peer by `TokenBucket` (`1.0` token/s, burst `5`, observable as `rate_limited_count` in `list-peers`). Unfocused `OSC 133;C` to `OSC 133;D;<exit>` commands running `>= long_command_secs` also route through `CompositeNotifier` with structured titles/priorities and **never** include `CommandRecord.output` or scrollback text. On the inbound path, `HavenPeerClient` **never** calls Haven's `EVERY_CALL`-gated `list_notifications` unless `inbound_shade_allowlist` is non-empty and a Mutiny window is active.

`DesktopNotifier` is a trait so tests inject a recorder and assert forwarding without a session bus. The badge comes from `AttentionReason::OscNotification`, the same mechanism as bell and agent notices, so focusing the pane clears it.

When testing the forwarding path, emit the sequence through the PTY (`write_to_pty` with a shell `printf`) so it arrives via `drain_pty`. Calling `advance_bytes` directly sets attention but bypasses the notification pipeline, which is why a test doing that sees an empty recorder. And in a Rust byte string, `\033` is a literal backslash for the shell's printf to interpret; `\0` alone is a NUL because Rust has no octal escapes.

### Split ratios and drag gestures

`SplitNode.ratio` is applied to the `GtkPaned` and written back on `notify::position`, so a drag survives rebuilds. Use `connect_notify_local`, not `connect_notify`: the latter requires a Send closure and `AppController` is Rc.

Apply the ratio with a tick callback that waits for a non-zero allocation, not `connect_map`. `map` fires before size allocation, so applying there reads total 0 and silently leaves the default 50/50. Guard the write-back handler with an `applying` flag so applying a ratio does not round-trip back into the model.

`build_split_widget` takes a `tab_id` and a path of first(false)/second(true) choices identifying each split. Widgets are rebuilt on every topology change, so a path stays valid for a widget's lifetime.

Drag invokes the same model operations as the keyboard verbs: tab `DragSource`/`DropTarget` call `reorder_tab`, and the content area's `DropTarget` calls `detach_split`. Keep it that way so gesture and keyboard behavior cannot diverge. Ratio persistence is within the workspace model; cross-restart persistence is M2 session restore.

### Kitty graphics

`src/graphics.rs` is pure data (images, placements, chunked-transmission state) with no GTK dependency, so the engine owns it and the UI decodes. `GraphicsStore::generation` bumps on mutation and the view's texture cache clears when it changes, so images are not decoded per frame.

The APC scanner (`ESC _ G ... ST`) lives in `scan_queries` alongside CSI and DCS. Graphics payloads can be large, so the APC buffer is capped at `MAX_IMAGE_BYTES`, not `MAX_QUERY_LEN`. Replies are sanitized so a malformed payload cannot inject escapes back into the sender, and `q=1`/`q=2` suppress OK/error replies.

Placements anchor to `history_size + cursor.line`, the same absolute coordinate as OSC 133 markers. That scrolls correctly while history grows but drifts once scrollback is capped, a limitation shared with prompt markers.

Chunked base64 must split at 4-character boundaries; each chunk is decoded independently. A test that splits mid-quantum produces invalid padding, which is a test bug, not a protocol bug.

`present-media` is an Act verb: it reads a file from the mutiny host and places it inline. It is additive (envelope still version 1) and added two optional response fields, `image_id` and `placement_id`. Adding it touched the full chain: `IpcRequest` plus its four match arms, `variant_ordinal`, `verb_samples`, the app handler, `mcp_surface_for`, `call_tool`, `tool_definitions`, `CtlVerb`, and the dispatch probe.

### IME and font fallback

Each `TerminalView` owns a `GtkIMMulticontext` wired to its key controller with `set_im_context`, so keys are filtered through the system IM module (or GTK's simple context for compose and dead keys) before `on_key_pressed` sees them. Do not bypass that ordering: consumed keys must not reach the raw encoder. `commit` writes UTF-8 straight to the PTY (typed input, not bracketed paste); `preedit_changed` stores the in-progress string, which `render_snapshot` draws underlined at the cursor and clears on commit. `update_im_cursor_location` tells the IME where to put its candidate window.

Font fallback is Pango's, via fontconfig. Do not reimplement it. The test proves it engages by asserting zero `unknown_glyphs_count` for CJK/emoji/box-drawing and that a mixed Latin+CJK string resolves to two different fonts.

Two traps worth remembering. GTK must be initialized and used from one thread while libtest spawns a thread per test, so all GTK assertions live in a single test function. And a bare `pango::Context::new()` has no font map, which makes itemization a no-op and `unknown_glyphs_count` vacuously zero; use a real widget's `pango_context()`.

`recompute_grid_dimensions` returns early on a zero allocation. An unrealized view is 0x0, and resizing from that shrank the pane to the minimum 4x2 grid, wrapping every wide glyph and eating characters. Keep that guard.

### Config store and live reload

`src/config.rs` owns the key set, defaults, parsing, and clamping. `Config` is the raw serde shape; `ResolvedConfig` is what the renderer and spawner read, with colors parsed to `Rgb` and numbers clamped, so no per-frame parsing happens. `Overrides` carries the CLI flags.

Precedence is flag, then file, then default, implemented in `config::resolve`. `AppController` holds the resolved config plus the path, the overrides, and the last seen mtime. `reload_config_if_changed` runs from `poll_once`, throttled to `RELOAD_CHECK_INTERVAL` (250 ms), and re-applies the same overrides on every reload so a file edit cannot discard a launch flag.

What applies when: font, colors, palette, and padding apply immediately (`apply_config_to_views` calls `TerminalView::refresh_font`, which recomputes Pango metrics and resizes the grid and PTY). Geometry and `scrollback_lines` apply to new panes and windows only, because alacritty fixes `scrolling_history` at `Term` construction. `PaneEngine::spawn_pty` takes `scrollback_lines` for that reason; `new_headless` keeps the 10 000 default for tests.

Failure behavior is deliberate: unknown keys are rejected (`deny_unknown_fields`) so typos surface; a malformed file keeps the previous config and does not retry until mtime changes again, so a half-written file cannot spam warnings; a bad color falls back per key; a palette that is not 16 entries falls back whole. Add new tunables to the store rather than as constants beside it.

`tests/config_reload.rs` uses explicit paths via `AppController::with_config`, so it needs no env mutation and runs in parallel. Reload tests must `settle()` past the 250 ms throttle and force a distinct mtime rather than relying on filesystem granularity.

### Shell integration is bundled and injected at spawn

`src/shell_integration.rs` holds the snippets as `include_str!` assets from `src/shell_integration/`, so the binary is self-contained. `PaneEngine::spawn_pty` resolves the shell program (the override, else `$SHELL`), classifies it by basename, materializes the assets under `integration_dir()`, and applies an `Injection` of extra argv and env. When the shell is not one we ship for (dash, sh, a custom program), or injection fails, it falls through to alacritty's own default resolution, so existing behavior is unchanged. Injection is best-effort: an unwritable runtime dir logs a warning and costs the markers, never the pane.

No dotfile is edited. Each shell gets a wrapper that loads the user's config first: bash `--rcfile bash_rc.sh`, zsh a `ZDOTDIR` set (`.zshenv`, `.zprofile`, `.zshrc`) that sources the originals by absolute path and restores `ZDOTDIR` at the end of `.zshrc`, fish `-C source`, nu a generated `--config` wrapper with the user's config path baked in as a literal (nu's `source` needs one). A caller's own `--rcfile`, `-C`, or `--config` wins over ours.

Two shell facts worth remembering when editing snippets: fish 4 emits OSC 133 natively, so the snippet version-gates on `$FISH_VERSION` and contributes only OSC 7 on 4.x, otherwise every marker would be duplicated and the engine would double its command records. nu has no `char escape`; ESC is `char -i 27`, and `hostname` is not guaranteed to exist, so use `(sys host).hostname`.

`tests/shell_integration.rs` is its own test binary with a single test because it sets process-wide `HOME`, `SHELL`, and `XDG_RUNTIME_DIR` to simulate a fresh install. Keep it that way: adding a second test to that file would race on the environment.

### The quick terminal

`toggle_quick_terminal` summons or dismisses one scratch window. `WindowModel.quick` marks it, and that flag is persisted with the layout, so a restore brings it back dismissed. `prepare_quick_window` sets `hide_on_close`, and the window's `close_request` handler checks the flag: a WM close calls `mark_quick_dismissed` and lets GTK hide it, while the explicit `close_window` verb still destroys it. Keep those two paths distinct; collapsing them either kills the scratch shell on a WM close or makes Close Window a no-op.

The summoned flag is `quick_visible: Cell<bool>` on the controller, not a read of `GtkWindow::is_visible`, so the toggle state machine is testable headlessly. The GTK calls are guarded on a view existing.

It is a plain shell regardless of `sessions.preferred`: a scratch pane should not leave a multiplexer session behind. `SpawnTab` and the IPC surface are unchanged.

There is no keep-above. GTK4 removed `set_keep_above` and `set_skip_taskbar_hint`, so summoning raises the window with `present()` and that is the whole of the stacking behaviour. Do not write docs claiming otherwise.

### Grid coordinates

In alacritty, `Line(0)` is the top of the visible screen and negative lines are scrollback; `display_offset` is how far the viewport is scrolled up. Because `history_size` grows as lines scroll off, prompt markers and command-output bounds are stored as `line_from_history_top = history_size + cursor.line.0`, an absolute coordinate stable across scrolling. `jump_prev_prompt`, `jump_next_prompt`, and `extract_command_output` all translate between the two. Mixing them up silently targets the wrong line.

### Turn tracking for agents

Two independent counters: `prompt_seq` (bumped by OSC 133;A) and `output_seq` (bumped per non-empty `advance_bytes` chunk). `has_turn_settled` is true when a new prompt arrived, or output arrived and then stayed quiet for `settle` while no command is running. `await-turn` parks baselines for both, which is how agents drive raw-mode TUIs that emit no shell integration. `send-to-agent` writes a bracketed-paste-sanitized turn plus `\r` and returns the counters to wait on.

Bounds worth knowing: 10 000 lines scrollback, 64 command records, 256 prompt markers, 8 KiB max OSC payload, 8-slot exited-pane ring, 256-entry audit ring.

### Consent and audit: the carrier assigns, the client never claims

`ConsentTier` (`Observe` / `Act` / `PolicyChange` / `Irreversible`) is per-verb, declared in `IpcRequest::consent_tier()`; `RunAction` delegates to the `ActionSpec`. `InstallPolicy`, `RevokePolicy`, `PairHaven`, `EnrollHavenSigner`, and `RevokePeer` are `PolicyChange`, and `EndSession` and `Quit` are Tier-4 `Irreversible`: both tiers always require explicit per-action confirmation (only `ConsentMode::Operator` is the consent itself; `Ask` and `Paired` both park for an in-window prompt with `Allow Session` hidden, or relay to a connected signer-enrolled Haven limb over HMAC-SHA256 where even `"session"` on the phone is enforced strictly as one-shot). `ActionId::InsertPeerTotp` is additionally refused outright on `CarrierRole::Peer` before consent parking so a remote agent can never dump live TOTP codes into a PTY, and `answer_auth_prompt` is triggered exclusively from physical GTK4 widget interaction on the Haven Auth Bar (`submit_haven_auth_prompt` is `pub(crate)`, with zero IPC/CLI/MCP password verbs). `ConsentMode` (Operator/Paired/Ask/ObserveOnly) is per-**carrier**, not per-client, and every remote HTTP peer resolves to `CarrierRole::Peer`, never `CarrierRole::Operator`. `CallerMeta` carries only a label; `consent_mode` is not a wire field, and `parse_envelope` refuses any envelope that sends one.

`AppController` binds two listeners. `accept_carrier` tags each accepted connection with its carrier's mode before dispatch:

- **Operator socket** (`start_ipc_listener`): assigns `Operator` to a same-uid peer. `mutiny ctl` arrives here.
- **Agent socket** (`start_agent_listener`, mode pinned by `--agent-consent`, default `Ask`): assigns that mode to every connection. `mutiny mcp` arrives here. The bridge's own `--consent` is a local ceiling enforced in `call_ipc` before forwarding, so it can restrict but never escalate.

`handle_accepted` checks `SO_PEERCRED` first and refuses a peer whose uid differs from `server_uid()`. Only then does it read the envelope. The matrix in `evaluate_consent_and_dispatch`:

- Observe verbs always pass. Act passes for Operator and Paired.
- Act + ObserveOnly is rejected. Act + Ask parks the stream and raises the in-window consent bar (25 s deadline), unless the **peer identity** already holds a session grant.
- Ask fails closed when no GUI window exists to present the prompt.

The matrix itself is `ipc::consent_outcome`, a pure function of (verb tier, carrier mode, `PeerIdentity`, grant set, gui_present). It deliberately takes no client label, so a claimed name cannot influence the decision. Keep policy there rather than inline in the controller: it is what the headless tests exercise.

**Session grants key on `PeerIdentity`, not on the label.** `PeerIdentity { uid, pid, start_time }` comes from `SO_PEERCRED` plus `/proc/<pid>/stat` field 22. Start time is included so a recycled pid does not inherit a dead process's grant; it reads 0 if `/proc` is unavailable, which degrades reuse protection but still separates live processes. A grant lives as long as the granted process, so restarting an MCP bridge requires re-approval. `AppController::grant_session(peer, label)` is the operator-side action the consent bar's Allow Session button calls; it is not reachable from IPC. `session_grants()` returns display labels with pids for the audit dialog. The `client` label is display and audit text only.

Every decision appends to the durable trail at `default_audit_path()` (`$XDG_STATE_HOME/mutiny/audit.jsonl`), recording the carrier and the `peer` identity alongside the verb tier. `redacted_summary()` logs text payloads as byte counts, never content. The file rotates past 8 MiB keeping one generation; `load_audit_tail` reloads the last 256 entries on startup and continues the sequence. Tests use `AppController::with_audit_path` to avoid writing to the real state directory.

One limit to keep in mind: same-uid processes are mutually trusted under the Unix model, so the operator socket grants Operator to any process running as the user. The uid check closes the cross-user hole, not that one.

Testing grant behavior needs **two real processes**. Every thread in one process shares a pid, so in-process `McpBridge` instances all have the same peer identity. `tests/e2e_ipc_pty.rs` has a `PeerProcess` helper that re-invokes the test binary (`--exact raw_peer_helper`) as a stdin-driven raw socket client; `raw_peer_helper` is a no-op when its env var is unset, so the normal suite stays headless-green.

### Search is chunked across ticks

A whole 10 000-line scan costs about 3.2 ms, most of the 4 ms tick, so nothing on the main loop scans whole. `ScrollbackSearch` is a resumable bottom-up cursor; `PaneEngine::scan_search_chunk` advances it by at most `SEARCH_LINES_PER_TICK` (512) lines. `AppController::advance_searches` runs in `poll_once` and divides that budget across every active scan, so N concurrent searches still cost one budget total.

Two holders, one code path: the pane's own slot (`begin_search` / `advance_search`) drives the viewport bar, and `PendingSearch` holds a separate cursor for an in-flight IPC `search` so the two do not clobber each other. The IPC reply is parked across ticks like `wait`; the request and response shapes are unchanged.

Grid line indices are relative to the current screen, so output arriving mid-scan invalidates the cursor and every collected match. `advance_external_search` compares `output_seq` and restarts the scan when it moved, which means a flooding pane shows "scanning" rather than stale results.

`search_scrollback` still exists with its original signature and runs to completion; it is for callers that are not tick-bound, and the bench measures it. Do not call it from `poll_once` or a widget.

### Tier 3 standing policies

`src/policy.rs` is pure: `policy_match` and `policy_decision` take the clock and the call history as arguments, so scope, pins, ceiling, and expiry are all testable without sleeping. The controller consults `policy_decision` BEFORE `consent_outcome`, and only for ask + Act, so a policy substitutes for a prompt and can never escalate a carrier.

`propose_policy` is Observe because asking is not acting; it queues an inert proposal. `StandingPolicy::sanitised` and `policy_match` reject policy-management verbs (`propose-policy`, `list-policies`, `install-policy`, `revoke-policy`), Tier-4 `Irreversible` verbs (`end-session`, `quit`), and unknown verb slugs. `install_policy` and `revoke_policy` never ride on an `Allow Session` grant, so an agent on an `ask` carrier cannot self-install or revoke policies. Policies bind to `PeerIdentity`, taken from `dispatch_peer` so an HTTP peer's token-derived identity is used rather than this process's pid.

`IpcRequest::pin_fields` supplies the exact-match values a policy can pin. Verbs without meaningful scalars return an empty map, so a pin on a key the call does not carry does NOT match vacuously.

Rate limiting is a rolling 60 s window per policy, pruned and bounded in `prune_uses`. Beyond the ceiling `policy_decision` returns None, so the caller falls through to prompting rather than allowing.

### Closing a view versus ending a session

With a multiplexer these are different operations and the vocabulary must not blur them. `close_pane` / `close_tab` / `close_window` end a CLIENT when mux-backed (so the multiplexer session keeps running), whereas on a plain shell they exit that shell. `end_session` runs the multiplexer's kill command and stops everything inside it, on every device. Menu labels reflect the actual backing: attached tabs, panes, and windows show `"Close Tab (session keeps running)"` / `"Close Pane (session keeps running)"` / `"Close Window (sessions keep running)"`, while plain shells show plain `"Close Tab"` / `"Close Pane"` / `"Close Window"`, and End Session sits in its own menu section behind a confirmation whose default button is the safe one.

`ListSessions { unattached_only }` is a VIEW for deliberate pruning, not a cleanup. The picker marks a live tmux session with no clients as "no clients" so an orphan is distinguishable from an attached one; zellij reports no count, so it is not guessed at.

Do not auto-end sessions when a tab closes. Persistent sessions that outlive their viewers are the reason to use a multiplexer at all; most of a real user's unattached sessions are deliberate, not abandoned.

Avoid "detach" for moving a pane to another window: to a tmux user detaching means detaching a client. The action is titled "Move Pane to New Window" and the tab item "Move to New Window". The `detach-split` SLUG is unchanged, because slugs are contract.

`build_tab_menu_model` uses `append_section`, so items are nested. A test walking the menu must follow `item_link(i, "section")` or it sees only sections and concludes the menu is empty.

### Session management: create, rename, and the tab menu

`create_argv` is separate from `attach_argv` on purpose. Attach passes tmux `-A` and zellij `attach --create`, both of which ATTACH on a name collision; create must fail instead, so it uses `new-session -s NAME` without `-A` and zellij's global `--session NAME`. Reusing the attach argv for creation is the bug this split prevents.

`rename_session` and `end_session` run the multiplexer's own command as a one-shot child, polled by `advance_mux_commands` with the same nonblocking drain and kill-then-reap rules as discovery. IPC callers park their reply stream on `PendingMuxCommand` so non-zero exit statuses and stderr propagate back as `IpcResponse::Error` rather than reporting premature success. The pane refs are updated only AFTER a rename exits successfully, so a failed rename never leaves mutiny claiming a name the multiplexer rejected. `apply_rename` updates every pane matching (kind, host, old name), not just the one that was right-clicked.

`ActionId::NewTab` honours `preferred_mux`; the IPC `SpawnTab` verb deliberately does not, because agents and scripts ask for a shell explicitly.

**What a GUI test can and cannot reach.** GtkModelButton is not a GtkButton in GTK4 and does not expose its action publicly, so a test cannot click a popover item through the public API. Driving `activate_action` on the window works but bypasses the popover, which is exactly why it passed while every item was dead. The reachable pair is: assert the popped-up menu built real GtkModelButton widgets with the expected labels (`rendered_menu_items` walks the widget tree), and assert the menu keeps its parent after a popdown so those items can resolve their actions. Both were mutation-checked. Pointer synthesis via xdotool was unreliable for popovers and is not the answer.

**The tab context menu is parented to the WINDOW and reused.** The first version created a popover per right-click, parented it to the tab button, and unparented it on `closed`. Every item then did nothing: GTK emits `closed` BEFORE activating the action, so unparenting removed the widget path the action resolves through. The terminal view's menu has always used parent-once-and-reuse; the tab menu now does too, swapping its model per tab and translating the gesture's button-relative coordinates with `compute_point`. `tests/tab_actions.rs` asserts the menu stays parented after a popdown, which is the invariant that broke.

**A parameterised gio action needs its target on the MENU ITEM**, not just the action name. `menu.append(label, action)` on an action that takes a parameter renders the item insensitive and it never fires. `build_tab_menu_model(attached, tab)` uses `MenuItem::set_action_and_target_value`. A screenshot cannot distinguish "rendered but dead" from working, which is why `tests/tab_actions.rs` drives the real actions with `activate_action` instead of trusting the picture.

Tab buttons carry no tooltip: the two-line label already shows name and host, and a tooltip window stacks over the right-click menu and hides items.

### Mouse reporting: the program owns the mouse when it asks

`PaneEngine::mouse_mode()` reads alacritty's `TermMode::MOUSE_MODE`. When it is set, the wheel and left press/drag/release must be encoded and written to the PTY, NOT applied to mutiny's scrollback and selection. Getting this wrong is what made tmux scrolling look broken: the wheel handler always scrolled locally and returned `Stop`.

`engine::mouse_report` is the pure encoder (SGR 1006 `ESC [ < Cb ; Cx ; Cy M`/`m`, legacy X10 `ESC [ M` plus three bytes offset by 32, coordinates 1-based and capped at 223). `engine::wheel_action` is the pure decision, extracted so it is testable without synthesising a GTK scroll event. Shift is the conventional override back to local behaviour, for both wheel and selection.

A scroll event carries no coordinates, so an `EventControllerMotion` tracks the pointer and the wheel reports the cell under it.

### Session discovery runs on its own, not only when the picker opens

The Sessions submenu used to be empty until the picker window was opened, because that was the only caller of `begin_discovery`. Discovery now starts in `connect_activate` and re-runs every `sessions.refresh_interval_s` (default 15, 0 disables) from `poll_once` while windows exist. This polls an external listing command; it owns no sessions and starts no daemon.

`begin_discovery` must NOT clear `cache.sessions`. Each target's result replaces its own (host, kind) slice in `record_discovery_result`, so keeping the old sessions until then is what stops the submenu flashing "No sessions found" at the start of every refresh.

### Menu IA: five archetypes, one home per action

`MenuCategory` is Session / Edit / Pane / Haven / View. Session is session-context and window lifecycle (grouped into create/attach, windows/layout, and close sections), Edit is the focused pane's CONTENT and settings (clipboard, find/prompt navigation, settings), Pane is STRUCTURE and focus, Haven is cross-limb actions, TOTP, pairing, and peer alerts, and View is chrome and introspection. The old File("Shell")/Pane("Panes")/View split overlapped, and Help held one item.

`tests/menu_ia.rs` carries a golden slug/chord/home table. Slugs and chords are contract: agents call `run_action` by slug and humans have muscle memory, so the IA change must not move them. Category is presentation and is not on the wire (`ActionCatalogItem` has no category field).

The Sessions submenu is a `gio::Menu` returned by `build_menubar_model()` alongside the root, registered with `AppController::register_sessions_menu`. Both the app menubar and each window's own `PopoverMenuBar` register theirs, and `refresh_session_pickers` fills them all when the inventory generation moves. Items use the parameterised `win.open-session-target` action, which is deliberately NOT in `ACTIONS`: its target is a session, not a keybind-able verb. Targets are kind/host/name tab-separated (`session_target` / `parse_session_target`) so names with spaces or punctuation survive.

### Labels: title is the session name, subtitle is the host

Haven's convention. `PaneEngine::mux_session` holds a `MuxSessionRef { kind, name, host }`; `title()` returns the session name when attached, and `MuxSessionRef::subtitle()` returns the SSH host or `mux::local_hostname()`. Tabs and picker rows are two-line, title prominent, subtitle dim and smaller, title ellipsised. No session index: it is redundant once sessions are named.

`tab_buttons` stores the button AND its two labels. `refresh_tab_strip_labels` must update those labels, never call `btn.set_label`, which would replace the two-line child with a plain string and silently undo the structure. `tab_labels()` reads the title label for the same reason.

### Settings dialog

`show_settings_dialog` builds one row per config key, grouped by TOML table. Each row contributes a `ConfigSetter` closure that reads its widget and writes its key, so Save starts from `config::load_or_default`, applies every setter, and writes `config::to_toml`. Adding a config key means adding one row; `tests/settings_dialog.rs` counts editable widgets against the key count so a missed row fails the build. The config store stays the single source of truth for the key set.

### App icon and desktop identity

`src/desktop.rs` embeds the SVG and seven PNGs with `include_bytes!`, the same self-contained approach as the shell-integration snippets. `ensure_icon_search_path` stages them into `$XDG_CACHE_HOME/mutiny/icons` in hicolor layout and the GUI adds that to the GTK icon theme, so a source checkout shows its icon with no install step. `mutiny install-desktop` writes the same bytes into the user's data home plus the `.desktop` entry, and is idempotent (contents compared before writing, so icon caches are not churned).

`APP_ID` is the single source for the application id, icon name, desktop basename, and `StartupWMClass`. **X11 `WM_CLASS` comes from the program name, not the application id**, so `glib::set_prgname` must be called with `APP_ID` before windows exist; otherwise `WM_CLASS` is "mutiny", the launcher does not match the running window, and the taskbar grows a second iconless entry. Verified with `xprop`: `WM_CLASS` is `dev.mutiny.Terminal` and `_NET_WM_ICON` carries the 16 px render.

The icon is set both globally (`Window::set_default_icon_name`) and per window (`.icon_name(...)`), because the global default is not observable through the per-window getter and X11 reads the window property.

Two icon states are embedded: idle (`APP_ID`) and agent-active (`AGENT_ICON_NAME`). `agent_active()` is `active_agent_badge().is_some()`, deliberately the SAME signal as the badge so they cannot disagree; the badge's 15 s freshness window is also the debounce. `refresh_agent_icon` runs in `poll_once` but only touches GTK on a transition, and `ensure_ui_window` applies the current state to a new window immediately so it does not start idle mid-session. The active variant shares the idle geometry exactly, changing only fills, which a unit test asserts.

Icon assets are generated, not hand-drawn per size: edit `assets/icon/dev.mutiny.Terminal.svg` and re-render. Keep to plain shapes and flat fills, which is both the Haven design language and what every SVG renderer agrees on.

### The tab strip wraps; it must never size the window

`tab_buttons_box` is a `gtk4::FlowBox`, not a horizontal `Box`. A Box's minimum width is the SUM of its children, so seventeen tabs pushed the toplevel's minimum from 618 to 2146 px and stretched the window. A FlowBox's minimum is its widest single child, so it wraps inside the available width instead. Selection is `None` because the buttons carry the active class themselves.

Wrapping trades width for height, so the FlowBox sits in a `ScrolledWindow` with horizontal scrolling off and `max_content_height` from `geometry.tab_strip_max_height_px`. Without that cap thirty-seven tabs allocated 436 px of strip and squeezed the terminal. `propagate_natural_width` stays false (a true value would reintroduce the width bug); `propagate_natural_height` is true so the strip claims up to the cap rather than collapsing.

`scroll_active_tab_into_view` runs on a tick callback, like the split-ratio restore, because allocations are not ready during `rebuild_from_model`. Use `compute_bounds`, not the deprecated `WidgetExt::allocation`.

`tests/tab_strip.rs` asserts the minimum width grows by at most one tab and then PLATEAUS as more tabs are added, that rows increase, that the visible strip respects the cap while its content overflows, and that clicking a wrapped tab still focuses it. The plateau assertion is the discriminating one: a sum-of-children layout cannot pass it.

### Text is placed per cell, not per run

The cursor is drawn at `grid.cursor.point * cell_size`, which was always correct. The bug was the TEXT: a style run drawn as one Pango layout uses the font's natural advance (8.83 px for Monospace 11) while the grid uses `ceil(approximate_char_width)` = 9 px. The shortfall accumulated per column, so a line ended about 0.17 px per column short of its grid span and the cursor looked like it drifted past the end.

The text pass therefore draws one glyph cluster per cell at `padding + col * cw`, including the cell's `zerowidth()` combining marks (which the run path dropped). This is exact for any font, including fallbacks whose advance differs. Do not "optimise" it back into runs without also forcing the advance per cluster.

Cost and its paydown, measured: per-cell placement first cost 646.9 -> 1741.6 us on a full repaint. A layout cache keyed on (cluster, bold, italic) brought it to 451.7 us, faster than the original run-based path while keeping exactness. Clear that cache whenever the font or config changes, since cached layouts carry the old font description. See benches/BASELINE.md.

`record_glyph_positions(true)` makes the next frame log (row, col, x) for every glyph, which is how `tests/cursor_grid.rs` proves the grid invariant. It is off in normal use.

### Rendering

`TerminalView` is a `gtk4::Widget` subclass implementing `WidgetImpl::snapshot`, drawing into `GtkSnapshot` render nodes. There is no `GLArea` and no per-pane GL context, so reparenting cannot tear down GPU state. Four passes: background runs, styled text runs via Pango layout, cursor, active-pane accent. Cell metrics come from Pango at construction; `size_allocate` → `recompute_grid_dimensions` → `PaneEngine::resize` → PTY `TIOCSWINSZ`. Panes spawn at 100×30 and are resized to fit on first allocation.

Selection follows the Linux model: drag sets primary selection, middle-click pastes it, `Ctrl+Shift+C`/`V` use the clipboard. Paste is sanitized against bracketed-paste escape injection in `format_paste`.

## Known gaps

These look closer to done than they are:

- **Kitty graphics is a slice.** No animation frames, file or shared-memory transmission, unicode placeholders, or cursor-movement policies. Placements drift once scrollback is capped, sharing the OSC 133 marker limitation.
- **The D-Bus notification send is untested.** CI has no session bus; forwarding is verified through an injected recorder, and the no-bus path is verified not to panic.
- **The Settings dialog writes the file; it is not a live editor.** It covers every key, but a malformed hand-edited file still falls back to the previous config, and there is no per-key validation feedback in the dialog.
- **Session restore is text-only and save-on-exit.** Restored scrollback loses colors and attributes; a crash loses changes since the last clean save. Mutiny-owned shells are never restored, by contract; multiplexer-attached panes re-attach because the multiplexer owns those processes.
- **The quick terminal is a summonable window, not a drop-down.** No layer-shell, so no always-on-top (GTK4 has no `set_keep_above` and Wayland has no protocol for it), no taskbar skip, and no animation. The chord works only while mutiny has focus; a system-wide global shortcut needs layer-shell or a per-compositor binding. Verified under Xvfb and live on KWin Wayland; a physical keypress was not synthesised.
- **Multiplexer panes get no shell integration.** tmux classifies as `ShellKind::Other`, so no OSC 133/7 injection; prompt jumping and command capture do not work inside them.
- **Only tmux and zellij are discovered.** screen, abduco, and dtach are not.
- **The HTTP MCP carrier is a slice.** No SSE, session management, or DELETE. Requests are stateless, so `initialize`'s client name does not persist. Identity, session grants, and consent mode are all per token. Driven end to end from Haven on a phone over an SSH local forward: unpaired 401, paired 200, a command sent and its output read back, `spawn_pane` and `present_notice` accepted, and both Act verbs denied when the carrier was pinned to `observe-only`. Also driven over a real WireGuard tunnel with the listener bound to the workstation's WG address, so both named transports are exercised.
- **Drag gesture wiring is untested.** The model operations and the divider write-back are tested; synthesizing a real drag needs a seat and compositor.
- **Tests skip when prerequisites are absent.** CI installs fish and asserts `fish --version`, and runs under Xvfb with CJK/emoji fonts, so the fish, IME, and graphics render tests execute there. Locally the fish test needs `MUTINY_TEST_FISH` or fish on PATH, and the GTK tests need a display.
- **The release workflow is unexercised.** `.github/workflows/release.yml` is tag-triggered and gated on CI, but no tag has been pushed.

CI itself runs green on GitHub at `GlassOnTin/mutiny` (public, AGPL-3.0-or-later): fmt, clippy, the displayless suite, the suite under Xvfb, and a release build in an Arch container, with cargo caching keyed on rustc and GTK versions. `./scripts/ci.sh` runs the same checks locally. It sets `WAYLAND_DISPLAY=` and `DISPLAY=` empty rather than unsetting them: unsetting makes GTK fall back to the default `wayland-0` socket and connect to the user's session, which is not displayless.
