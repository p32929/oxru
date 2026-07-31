#!/bin/sh
# Build the current local source and RUN it in place — without installing
# anything to your system (no ~/.cargo/bin CLI, no /Applications/Oxru.app).
#
# Usage:
#   ./run-local.sh                 # TUI, opens the current folder
#   ./run-local.sh .               # same, explicit
#   ./run-local.sh --gui .         # GUI window, current folder
#   ./run-local.sh ~/some/project  # TUI, opens that folder
#   ./run-local.sh --gui ~/proj    # GUI, opens that folder
#
# Everything after the script name is forwarded straight to the oxru binary,
# so any flag oxru understands works here too. Fast iteration:
#   OXRU_DEBUG=1 ./run-local.sh .  # debug build (compiles faster, runs slower)
set -eu

# Always build from the repo root, wherever this is invoked from.
cd "$(dirname "$0")"

blue() { printf '\033[0;34m%s\033[0m\n' "$1"; }
red()  { printf '\033[0;31m%s\033[0m\n' "$1"; }

if ! command -v cargo >/dev/null 2>&1; then
    red "Need 'cargo' — install Rust from https://rustup.rs"
    exit 1
fi

# Release by default (matches the installed build); OXRU_DEBUG=1 for a quicker
# compile while iterating.
if [ "${OXRU_DEBUG:-0}" = "1" ]; then
    blue "Building debug binary…"
    cargo build >/dev/null
    BIN="target/debug/oxru"
else
    blue "Building release binary…"
    cargo build --release >/dev/null
    BIN="target/release/oxru"
fi

# With no arguments, open the current directory (the most useful default for a
# quick try). Otherwise forward exactly what was given.
if [ "$#" -eq 0 ]; then
    set -- .
fi

exec "$BIN" "$@"
