#!/usr/bin/env bash
# Sets up the build environment and builds coffer from source.
#
# System packages are installed through whichever package manager the
# distro has - apt (Debian, Ubuntu, Raspberry Pi OS, Mint, ...), dnf
# (Fedora, RHEL, AlmaLinux, Rocky, Oracle) or zypper (openSUSE Leap and
# Tumbleweed). The package lists are the same ones the release pipeline
# builds the .deb/.rpm packages with on each of those distros, so they are
# known to resolve there. Everything after that step (rustup, the build,
# the install prompt) is distro-independent.
set -euo pipefail
cd "$(dirname "$0")"

# Root (e.g. inside a container, or the script itself run via sudo) needs
# no sudo in front of the package manager - and may not even have it.
SUDO=sudo
[ "$(id -u)" -eq 0 ] && SUDO=""

# No libfuse development package: fuser's pure-Rust mount links no libfuse
# (see Cargo.toml). `fuse3` is still needed for the fusermount3 helper that
# mount/umount shell out to at runtime. perl and make are for OpenSSL,
# which is compiled from source (see below).
echo "==> Installing system packages (build tools, fuse3)"
if command -v apt-get >/dev/null 2>&1; then
    $SUDO apt-get update -qq
    $SUDO apt-get install -y \
        build-essential perl fuse3 curl ca-certificates
elif command -v dnf >/dev/null 2>&1; then
    # --allowerasing: EL installs often carry curl-minimal, which conflicts
    # with the full curl package and would otherwise abort the whole step.
    $SUDO dnf install -y --allowerasing \
        gcc make perl fuse3 curl ca-certificates
elif command -v zypper >/dev/null 2>&1; then
    $SUDO zypper --non-interactive install --no-recommends \
        gcc make perl fuse3 curl ca-certificates
else
    echo "No supported package manager found (apt-get, dnf or zypper)." >&2
    echo "Install manually instead: a C compiler, make, perl, fuse3 (for fusermount3), curl -" >&2
    echo "then a Rust toolchain via https://rustup.rs and run: make build" >&2
    exit 1
fi

# SQLCipher and OpenSSL are compiled from source and statically linked in
# (see Cargo.toml: rusqlite's bundled-sqlcipher-vendored-openssl feature).
# No libsqlcipher-dev/libssl-dev needed - this also sidesteps a confirmed
# upstream SQLCipher bug present in distro-packaged builds (see the "Known
# upstream SQLCipher bug" section in README.md). perl+make (make comes with
# build-essential) are needed to build OpenSSL from source.

# The Rust toolchain shipped by the distros themselves (1.63-1.75 on
# Debian/Ubuntu depending on release) is too old for current crate
# dependencies (several now require the 2024 edition). Always use rustup's
# current stable toolchain instead, kept under $HOME/.cargo - it does not
# conflict with any distro rustc/cargo.
if ! command -v rustup >/dev/null 2>&1; then
    echo "==> Installing Rust via rustup (the distro's rustc is too old for this project's dependencies)"
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
