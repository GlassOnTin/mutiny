# Engine and render latency baseline

Recorded 2026-11-19, re-measured after the damage-tracking rung that closed M1.
Reproduce with `cargo bench`; the render rows need a display, so run it under
Xvfb (`GDK_BACKEND=x11 xvfb-run -a cargo bench`).

## Environment

- AMD Ryzen 9 9950X, 32 threads, Linux 7.0.0-38-generic
- rustc 1.97.1, `bench` profile (optimized)
- Engine rows: headless, 120x40 grid. Render rows: Xvfb with the X11 backend,
  1200x800 widget, 43-row pane.

## Numbers

Median and p95 over the iteration counts in `benches/latency.rs`.

| Measurement | Median | p95 | Note |
| --- | --- | --- | --- |
| VT ingest | 2655 us | 4055 us | 184 KiB mixed stream, 67.8 MiB/s |
| Input bytes to grid | 0.1 us | 0.1 us | engine only, confirmation read excluded |
| PTY echo round trip | 9.6 us | 16.7 us | real `/bin/sh` PTY, 0 of 40 failed |
| Pane snapshot | 6.7 us | 12.5 us | one agent poll on a populated grid |
| Scrollback search, hit | 4.7 us | 6.6 us | 20 matches near live screen, early exit |
| Scrollback search, miss | 3234 us | 3370 us | full 10 000-line scan, worst case |
| Search chunk (512 lines) | 223 us | 233 us | one tick's worth of the chunked scan |
| Render, idle frame | 4.9 us | 5.2 us | 1 of 43 rows rebuilt (the cursor line) |
| Render, changed frame | 36.5 us | 42.7 us | 2 of 43 rows rebuilt |
| Render, full repaint | 646.9 us | 706.4 us | 43 of 43 rows, the pre-damage path |

Run-to-run variance on VT ingest was about 15% across two runs (2309 us then
2655 us median) on an otherwise idle machine. Treat differences under roughly
20% as noise unless they reproduce.

## Damage tracking result

GtkSnapshot is immediate-mode: every `snapshot()` call rebuilds a render-node
tree for the whole widget, so there is no partial framebuffer update to be had.
The win comes from caching a render node per row and rebuilding only the rows
alacritty reports as damaged, then re-appending cached nodes for the rest.

Measured on a 43-row pane full of styled text:

- An idle frame rebuilds 1 row (alacritty re-marks the cursor line on every
  `damage()` call) and costs 4.9 us, against 646.9 us for a full repaint.
  Roughly 131x cheaper.
- A frame with one changed line rebuilds 2 rows and costs 36.5 us, roughly
  17.7x cheaper than a full repaint.
- Scroll, resize, font, config, selection, preedit, and graphics changes
  invalidate the whole cache, because they move or restyle content that line
  damage does not track. Those frames cost a full repaint, which is correct.

A blank row appends nothing and an empty snapshot yields no node, so each row
gets a transparent rect to guarantee a cacheable node. Without it every empty
line rebuilt on every frame.

## What this covers, and what it does not

Covered: the `PaneEngine` costs an agent or a keystroke actually pays. VT
parsing including the OSC side channel, key encoding plus grid update, a real
PTY echo round trip, snapshot serialization, and scrollback search.

Not covered:

- **GTK rendering to a real window.** The render rows above measure
  `render_snapshot` into a `GtkSnapshot` under Xvfb, which covers node
  construction and Pango shaping. They do not cover GPU compositing or
  presentation, which need a real display and a compositor.
- **End-to-end input latency under load.** The PTY echo figure is measured with
  a tight poll loop and an idle shell. It is a floor, not a loaded percentile.
- **IPC and MCP round trips.** Socket and JSON costs are not isolated here.

## Implication worth acting on (addressed)

A full scrollback scan costs about 3.2 ms, which is most of the 4 ms GUI tick.
Running it inline on the main loop with a needle that does not match drops a
frame.

The M1 chunked-search rung addressed this. `SEARCH_LINES_PER_TICK` is 512, and
both the viewport search bar and the IPC `search` verb advance a resumable
cursor by that budget per tick instead of scanning whole. Re-measured after the
change:

- One chunk: 223 us median, 233 us max, which is 5.8% of the 4 ms tick.
- A full-history miss spreads over 20 ticks, 4196 us total.

The trade is per-tick cost, not total cost. Across three runs the maximum
chunk was 176 to 233 us (4.4% to 5.8% of the tick), while the whole-scan miss
measured 3034 to 3674 us. Total wall time for the chunked miss was 2752 to
4196 us, which overlaps the whole-scan range: chunking does not measurably
increase total cost, it caps what any one tick spends. An earlier revision of
this file claimed a 14% total slowdown; that came from a single sample and did
not survive repetition.

The whole-scan `search_scrollback` wrapper is kept for callers that are not
tick-bound, and the whole-scan figures above are from it.

`engine::tests::one_chunk_cannot_scan_a_full_history` asserts the budget
deterministically, so the guarantee does not rest on a timing measurement that
a loaded CI machine could skew.
