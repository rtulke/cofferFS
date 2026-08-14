#!/usr/bin/env bash
# Sets up the build environment for the Rust cryptc rewrite.
# Tested targets: Ubuntu 24.04, Ubuntu 26.06, Debian 12 (bookworm), Debian 13 (trixie).
set -euo pipefail
cd "$(dirname "$0")"

if ! command -v apt-get >/dev/null 2>&1; then
    echo "This script only supports apt-based systems (Ubuntu/Debian)." >&2
    echo "Install manually instead: build-essential, pkg-config, libfuse3-dev, libsqlcipher-dev, curl, then a Rust toolchain via https://rustup.rs" >&2
    exit 1
fi

echo "==> Installing system packages (build tools, fuse3)"
sudo apt-get update -qq
sudo apt-get install -y \
    build-essential \
    pkg-config \
    perl \
    fuse3 \
    libfuse3-dev \
    curl \
    ca-certificates

# SQLCipher and OpenSSL are compiled from source and statically linked in
# (see Cargo.toml: rusqlite's bundled-sqlcipher-vendored-openssl feature).
# No libsqlcipher-dev/libssl-dev needed - this also sidesteps a confirmed
# upstream SQLCipher bug present in distro-packaged builds (see
# ../upstream-sqlcipher-bug/). perl+make (make comes with build-essential)
# are needed to build OpenSSL from source.

# The Rust toolchain shipped in apt on these distros (1.63-1.75 depending on
# release) is too old for current crate dependencies (several now require the
# 2024 edition). Always use rustup's current stable toolchain instead, kept
# under $HOME/.cargo - it does not conflict with any distro rustc/cargo.
if ! command -v rustup >/dev/null 2>&1; then
    echo "==> Installing Rust via rustup (apt's rustc is too old for this project's dependencies)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
else
    echo "==> rustup already installed, updating stable toolchain"
    rustup update stable
fi

# shellcheck disable=SC1090
source "$HOME/.cargo/env"

echo "==> Building cryptc (release)"
cargo build --release

echo
echo "Done. Binary at: target/release/cryptc"
echo "To use cargo/rustc in new shells:  source \"\$HOME/.cargo/env\""
echo "Build again any time with:         make build"
echo "Install system-wide (optional):    sudo make install"
