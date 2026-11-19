# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Linux desktop terminal in pure Rust + `gtk4-rs` (no libadwaita, no Ghostty/Zig/C-ABI code), designed as the workstation peer to Haven: humans and agents act on the same panes through the same verb vocabulary. One binary plays three roles: GTK GUI, `mutiny ctl` CLI, and `mutiny mcp` stdio MCP server.

`VISION.md` is the product and architecture spec (draft v11). M0, the trust/parity half of M0.5, peer-identity grant binding, chunked scrollback search, bundled shell integration, TOML config, terminal capability queries, and IME with font fallback are shipped. The next rung is Kitty graphics, then the rest of M1 (drag gestures, OSC 777, damage tracking). Read it for intent and constraints, not as a description of current behaviour. Its "Growth diagnostic" section is the authoritative list of which claims are ahead of the code.

Doc style: VISION.md follows the structure of Haven's `~/Code/Haven/VISION.md` (thesis, identity, numbered commitments, presence laws, growth diagnostic, juxtapositions, milestones, scope boundaries). Match that structure, but use plain punctuation. No em-dashes, including in section headings; use a colon or a full stop instead.

## Commands

```bash
cargo build                              # requires GTK 4.22+ dev libs (pkg-config gtk4)
cargo test                               # 71 tests: 44 unit + 7 config + 11 e2e + 1 ime + 1 shell + 7 queries
cargo test --test e2e_ipc_pty            # integration only (live PTYs, carriers, audit, MCP)
cargo test detach_split                  # single test, by name substring
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo bench                              # headless engine baseline, see benches/BASELINE.md
./scripts/ci.sh                          # everything CI runs, forced displayless
```

Edition 2024, declared `rust-version = 1.92`. Clippy (with `-D warnings`) and fmt are clean.

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

### Grid coordinates

In alacritty, `Line(0)` is the top of the visible screen and negative lines are scrollback; `display_offset` is how far the viewport is scrolled up. Because `history_size` grows as lines scroll off, prompt markers and command-output bounds are stored as `line_from_history_top = history_size + cursor.line.0`, an absolute coordinate stable across scrolling. `jump_prev_prompt`, `jump_next_prompt`, and `extract_command_output` all translate between the two. Mixing them up silently targets the wrong line.

### Turn tracking for agents

Two independent counters: `prompt_seq` (bumped by OSC 133;A) and `output_seq` (bumped per non-empty `advance_bytes` chunk). `has_turn_settled` is true when a new prompt arrived, or output arrived and then stayed quiet for `settle` while no command is running. `await-turn` parks baselines for both, which is how agents drive raw-mode TUIs that emit no shell integration. `send-to-agent` writes a bracketed-paste-sanitized turn plus `\r` and returns the counters to wait on.

Bounds worth knowing: 10 000 lines scrollback, 64 command records, 256 prompt markers, 8 KiB max OSC payload, 8-slot exited-pane ring, 256-entry audit ring.

### Consent and audit: the carrier assigns, the client never claims

`ConsentTier` (Observe/Act) is per-verb, declared in `IpcRequest::consent_tier()`; `RunAction` delegates to the `ActionSpec`. `ConsentMode` (Operator/Paired/Ask/ObserveOnly) is per-**carrier**, not per-client. `CallerMeta` carries only a label; `consent_mode` is not a wire field, and `parse_envelope` refuses any envelope that sends one.

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

### Rendering

`TerminalView` is a `gtk4::Widget` subclass implementing `WidgetImpl::snapshot`, drawing into `GtkSnapshot` render nodes. There is no `GLArea` and no per-pane GL context, so reparenting cannot tear down GPU state. Four passes: background runs, styled text runs via Pango layout, cursor, active-pane accent. Cell metrics come from Pango at construction; `size_allocate` → `recompute_grid_dimensions` → `PaneEngine::resize` → PTY `TIOCSWINSZ`. Panes spawn at 100×30 and are resized to fit on first allocation.

Selection follows the Linux model: drag sets primary selection, middle-click pastes it, `Ctrl+Shift+C`/`V` use the clipboard. Paste is sanitized against bracketed-paste escape injection in `format_paste`.

## Known gaps

These look closer to done than they are:

- **No config file.** VISION specifies TOML with live reload; there is no config module and no `toml` dependency. Font (`Monospace 11`), colours, and the 16-colour palette are constants in `src/ui/terminal_view.rs`.
- **Split ratios are stored but not applied.** `SplitNode::Split { ratio }` is preserved through tree surgery, but `build_split_widget` never calls `set_position`, and user drags are not written back. Splits render at GTK's default.
- **No shell integration snippets shipped.** OSC 133/7 tracking works only if the shell emits the markers; nothing in the repo installs them. PTYs do get `TERM_PROGRAM=mutiny` and `MUTINY_PANE_ID=<id>` for that purpose.
- **No in-app settings UI.** The TOML file is the only config surface.
- **Carried follow-up: assert `fish --version` in CI.** The fish startup test skips when fish is absent, so a run where the package install fails would report green without exercising it. CI installs fish today, but nothing asserts it succeeded.
- **The fish and IME tests skip when their prerequisites are absent.** CI installs fish and runs under Xvfb with CJK/emoji fonts so both execute. Locally, point `MUTINY_TEST_FISH` at a fish binary; the IME test needs a display.
- **Damage tracking is API-only.** `has_damaged_lines` and `reset_damage` have no callers; `render_snapshot` repaints every cell.
- **CI runs on GitHub and is green.** `origin` is `https://github.com/GlassOnTin/mutiny` (public, AGPL-3.0-or-later). `.github/workflows/ci.yml` runs fmt, clippy, the displayless suite, the suite under Xvfb, and a release build in an Arch container, with cargo caching keyed on rustc and GTK versions. `./scripts/ci.sh` runs the same checks locally. `.github/workflows/release.yml` is tag-triggered and gated on CI, but has not been exercised: no tag has been pushed.
- Untested by the current suite: real GUI rendering, Kitty graphics, drag gestures, session restore, and the M2 HTTP MCP listener.
