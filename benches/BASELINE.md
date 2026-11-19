# Engine latency baseline (M0.5)

Recorded 2026-11-19 so later changes compare against a fixed point rather than
a feeling. Reproduce with `cargo bench`.

## Environment

- AMD Ryzen 9 9950X, 32 threads, Linux 7.0.0-38-generic
- rustc 1.97.1, `bench` profile (optimized)
- Headless: no GTK, no display. Grid 120x40.

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

Run-to-run variance on VT ingest was about 15% across two runs (2309 us then
2655 us median) on an otherwise idle machine. Treat differences under roughly
20% as noise unless they reproduce.

## What this covers, and what it does not

Covered: the `PaneEngine` costs an agent or a keystroke actually pays. VT
parsing including the OSC side channel, key encoding plus grid update, a real
PTY echo round trip, snapshot serialization, and scrollback search.

Not covered:

- **GTK rendering.** `TerminalView::render_snapshot` needs a display and is not
  measured. It repaints every cell and builds a Pango layout per styled run,
  so it is the likely cost centre and the first thing to measure once a
  display-bearing benchmark exists. The damage API (`has_damaged_lines`,
  `reset_damage`) is still unwired.
- **End-to-end input latency under load.** The PTY echo figure is measured with
  a tight poll loop and an idle shell. It is a floor, not a loaded percentile.
- **IPC and MCP round trips.** Socket and JSON costs are not isolated here.

## Implication worth acting on

A full scrollback scan costs about 3.2 ms, which is most of the 4 ms GUI tick.
Running `search_scrollback` inline on the main loop with a needle that does not
match will drop a frame. The M1 viewport search bar should chunk the scan
across ticks or cap the lines scanned per tick rather than calling it whole.
