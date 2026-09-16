#!/usr/bin/env bash
# Builds one .deb per target distro, natively in that distro's own container.
#
# Why one per distro: every package is verified on exactly the distro it is
# for and declares that distro's own glibc version. Nothing else varies -
# SQLCipher/OpenSSL are statically bundled and fuser's pure-Rust mount links
# no libfuse (see Cargo.toml), so the only runtime dependencies are the C
# library and the fusermount3 helper from the fuse3 package. See the
# Packaging section in REFERENCE.md for the history behind this.
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
rm -f dist/coffer_*.deb

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
                build-essential perl curl ca-certificates git
            curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | \
                sh -s -- -y --default-toolchain stable --profile minimal
            source "$HOME/.cargo/env"
            cargo install cargo-deb --locked --version 3.7.0
            make man
            cargo build --release
            make completions
            cargo deb
        '
    # cargo-deb names the file after the architecture it was built on
    # (_amd64 on x86_64 hosts, _arm64 on aarch64 - the release workflow
    # builds both); the distro id is slotted in before that.
    SRC_DEB=$(ls "target-$id/debian/"*.deb | head -1)
    DST_DEB="dist/$(basename "$SRC_DEB" | sed -E "s/_([a-z0-9]+)\.deb$/_${id}_\1.deb/")"
    cp "$SRC_DEB" "$DST_DEB"
    echo "-> $DST_DEB"
done

echo
echo "Built packages:"
ls -la dist/
