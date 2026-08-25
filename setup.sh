#!/usr/bin/env bash
# Sets up the build environment for the Rust coffer rewrite.
# Tested targets: Ubuntu 24.04, Ubuntu 26.04, Debian 12 (bookworm), Debian 13 (trixie).
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
# upstream SQLCipher bug present in distro-packaged builds (see the "Known
# upstream SQLCipher bug" section in README.md). perl+make (make comes with
# build-essential) are needed to build OpenSSL from source.

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

echo "==> Building coffer (release)"
cargo build --release

BIN="target/release/coffer"
echo
echo "Done. Binary at: $BIN"
echo "To use cargo/rustc in new shells:  source \"\$HOME/.cargo/env\""
echo "Build again any time with:         make build"
echo

if [ "$(id -u)" -eq 0 ]; then
    # Already root (e.g. this script itself was run via sudo) - just install,
    # no need to ask and no sudo-into-a-locked-down-$HOME problem to dodge.
    echo "==> Running as root, installing to /usr/local/bin"
    install -Dm755 "$BIN" /usr/local/bin/coffer
    echo "Installed: /usr/local/bin/coffer"
elif [ ! -t 0 ]; then
    echo "Not installed (non-interactive shell, skipping the install prompt)."
    echo "Install later with:   make install PREFIX=\$HOME/.local   (no sudo)"
    echo "                  or: sudo make install                  (system-wide)"
else
    echo "Where should coffer be installed?"
    echo "  1) Just for you, no sudo needed  (~/.local/bin/coffer)"
    echo "  2) System-wide for all users     (/usr/local/bin/coffer, needs sudo)"
    echo "  3) Don't install it - I'll copy the binary myself"
    read -rp "Choice [1-3, default 1]: " choice || choice=3
    case "${choice:-1}" in
        1)
            make install PREFIX="$HOME/.local" >/dev/null
            echo "Installed: $HOME/.local/bin/coffer"
            case ":$PATH:" in
                *":$HOME/.local/bin:"*) ;;
                *)
                    echo "Note: ~/.local/bin isn't on your PATH yet. Add this to ~/.bashrc:"
                    echo "    export PATH=\"\$HOME/.local/bin:\$PATH\""
                    ;;
            esac
            ;;
        2)
            # Stage through /tmp (world-traversable) before handing off to
            # sudo: some setups don't let root read straight out of your
            # home directory (e.g. a $HOME locked to mode 700), which would
            # otherwise turn a plain `sudo install ...` into a confusing
            # "permission denied" failure.
            make man >/dev/null
            TMP_BIN="$(mktemp)"
            TMP_MAN="$(mktemp)"
            cp "$BIN" "$TMP_BIN"
            cp target/man/coffer.1.gz "$TMP_MAN"
            sudo install -Dm755 "$TMP_BIN" /usr/local/bin/coffer
            sudo install -Dm644 "$TMP_MAN" /usr/local/share/man/man1/coffer.1.gz
            rm -f "$TMP_BIN" "$TMP_MAN"
            echo "Installed: /usr/local/bin/coffer"
            ;;
        3)
            echo "Not installed. Run it directly:   ./$BIN create myvault.coffer"
            echo "Install later with:   make install PREFIX=\$HOME/.local   (no sudo)"
            echo "                  or: sudo make install                  (system-wide)"
            ;;
        *)
            echo "Unrecognized choice '$choice', not installing." >&2
            echo "Run it directly:   ./$BIN create myvault.coffer"
            ;;
    esac
fi
