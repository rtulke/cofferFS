#!/usr/bin/env bash
# Builds one .deb per target distro, natively in that distro's own container.
#
# Why not one universal package: Debian 12/Ubuntu 24.04 ship libfuse3 as
# SONAME 3 (package `libfuse3-3`), but Debian 13/Ubuntu 26.04 bumped it to
# SONAME 4 (package `libfuse3-4`, and critically it does NOT also provide a
# `libfuse3.so.3` symlink) - a binary linked against one SONAME can't
# dynamically load the other. Confirmed by testing: a Debian-12-built binary
# fails to install on Debian 13 at all (`Depends: libfuse3-3` isn't even
# resolvable there). So each target gets its own native build, same as this
# project's other packaging pipelines (see rtulke/rocket.chat-tray).
#
# SQLCipher/OpenSSL are still statically bundled in every build (see
# Cargo.toml), so libfuse3 is the only runtime library dependency that
# varies across targets.
set -euo pipefail
cd "$(dirname "$0")/.."   # repo root

RUNTIME=docker
command -v docker >/dev/null 2>&1 || RUNTIME=podman

TARGET_IDS=(debian12 debian13 ubuntu2404 ubuntu2604)
declare -A TARGET_IMAGES=(
    [debian12]=debian:12-slim
    [debian13]=debian:13-slim
    [ubuntu2404]=ubuntu:24.04
    [ubuntu2604]=ubuntu:26.04
)

mkdir -p dist
rm -f dist/cryptc_*.deb

for id in "${TARGET_IDS[@]}"; do
    img="${TARGET_IMAGES[$id]}"
    echo "=========================================================="
    echo "== Building for $id ($img)"
    echo "=========================================================="
    "$RUNTIME" run --rm \
        -e CARGO_TARGET_DIR="/work/target-$id" \
        -v "$PWD:/work:Z" \
        -w /work \
        "$img" \
        bash -euxc '
            apt-get update -qq
            apt-get install -y --no-install-recommends \
                build-essential pkg-config perl libfuse3-dev curl ca-certificates
            curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | \
                sh -s -- -y --default-toolchain stable --profile minimal
            source "$HOME/.cargo/env"
            cargo install cargo-deb --locked --version 3.7.0
            make man
            cargo deb
        '
    SRC_DEB=$(ls "target-$id/debian/"*.deb | head -1)
    DST_DEB="dist/$(basename "$SRC_DEB" | sed "s/_amd64/_${id}_amd64/")"
    cp "$SRC_DEB" "$DST_DEB"
    echo "-> $DST_DEB"
done

echo
echo "Built packages:"
ls -la dist/
