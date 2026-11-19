#!/usr/bin/env bash
# The same checks CI runs, locally. Tests are forced displayless so this
# reproduces the headless job rather than accidentally using your desktop.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== cargo fmt --check =="
cargo fmt --check

echo "== cargo clippy --all-targets -- -D warnings =="
cargo clippy --all-targets -- -D warnings

echo "== cargo test (no display) =="
# Empty, not unset: unsetting WAYLAND_DISPLAY makes GTK fall back to the
# default wayland-0 socket and connect to the user's session anyway. CI sets
# these empty, so match it exactly.
WAYLAND_DISPLAY= DISPLAY= GDK_BACKEND=x11 cargo test

echo "== cargo bench --no-run =="
cargo bench --no-run

echo "all checks passed"
