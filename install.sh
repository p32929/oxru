#!/bin/sh
# Oxru installer.
#
#   curl -fsSL https://raw.githubusercontent.com/p32929/oxru/master/install.sh | sh
#
# Builds Oxru from source with Cargo and installs the `oxru` binary into
# Cargo's bin directory (usually ~/.cargo/bin, which should be on your PATH).
set -eu

REPO="https://github.com/p32929/oxru"

red()   { printf '\033[0;31m%s\033[0m\n' "$1"; }
green() { printf '\033[0;32m%s\033[0m\n' "$1"; }
blue()  { printf '\033[0;34m%s\033[0m\n' "$1"; }

blue "Installing Oxru…"

# Cargo is required to build from source.
if ! command -v cargo >/dev/null 2>&1; then
    red "Couldn't find 'cargo'. Oxru is built from source, so you need a Rust toolchain."
    echo "Install one from https://rustup.rs and re-run this script:"
    echo
    echo "    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    echo
    exit 1
fi

# `cargo install --git` clones, builds in release, and drops the binary in
# Cargo's bin dir. `--force` makes re-running this an upgrade.
cargo install --git "$REPO" --force

echo
green "Done! 'oxru' is installed."

# A friendly nudge if Cargo's bin dir isn't on PATH yet.
BIN_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"
case ":${PATH}:" in
    *":${BIN_DIR}:"*) : ;;
    *)
        echo
        echo "Note: ${BIN_DIR} isn't on your PATH. Add this to your shell profile:"
        echo
        echo "    export PATH=\"${BIN_DIR}:\$PATH\""
        ;;
esac

echo
echo "Try it:    oxru --gui ."
