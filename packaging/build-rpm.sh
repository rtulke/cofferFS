#!/usr/bin/env bash
# Builds one .rpm per target distro, natively in that distro's own container,
# via cargo-generate-rpm. The RPM counterpart of build-deb.sh.
#
# Why one per distro and not a single "el9-built runs everywhere" package:
# every package is verified on exactly the distro it is for, and its
# automatically discovered `Requires:` names that distro's own glibc
# version. Nothing else varies - SQLCipher/OpenSSL are statically bundled
# and fuser's pure-Rust mount links no libfuse (see Cargo.toml), so the
# only runtime dependencies are the C library and the fusermount3 helper
# from the fuse3 package. See the Packaging section in REFERENCE.md.
#
# The distro id goes into the RPM's Release field (`1.fedora44`, `1.el9`,
# ...), so the file names come out as coffer-<version>-1.<id>.x86_64.rpm -
# the same spelled-out scheme as the .deb side, and without any renaming
# step since RPM naming puts the Release field into the file name itself.
set -euo pipefail
cd "$(dirname "$0")/.."   # repo root

RUNTIME=docker
command -v docker >/dev/null 2>&1 || RUNTIME=podman

CARGO_GENERATE_RPM_VERSION=0.21.0

# id -> image. The ids are kept as an ordered list (not just the map's
# keys) so the build order is deterministic.
TARGET_IDS=(fedora43 fedora44 el9 el10 leap160 tumbleweed)
declare -A TARGET_IMAGES=(
    [fedora43]=fedora:43
    [fedora44]=fedora:44
    [el9]=almalinux:9
    [el10]=almalinux:10
    [leap160]=opensuse/leap:16.0
    [tumbleweed]=opensuse/tumbleweed
)

# Per package-manager family. rpm-build is installed so cargo-generate-rpm's
# default `auto-req = "auto"` finds the distro's own /usr/lib/rpm/find-requires
# and emits exactly the versioned `Requires:` that distro's packages carry,
# instead of falling back to its ldd-based approximation. No fuse3-devel:
# nothing links libfuse (see Cargo.toml).
# --allowerasing: the EL images ship curl-minimal, which conflicts with the
# full curl package and makes a plain `dnf install curl` fail outright.
DNF_INSTALL='dnf install -y --allowerasing --setopt=install_weak_deps=False \
    gcc make perl rpm-build git curl ca-certificates tar gzip'
ZYPPER_INSTALL='zypper --non-interactive --gpg-auto-import-keys refresh && \
    zypper --non-interactive install --no-recommends \
    gcc make perl rpm-build git curl ca-certificates tar gzip'

mkdir -p dist
rm -f dist/coffer-*.rpm

for id in "${TARGET_IDS[@]}"; do
    img="${TARGET_IMAGES[$id]}"
    case "$img" in
        opensuse/*) install="$ZYPPER_INSTALL" ;;
        *)          install="$DNF_INSTALL" ;;
    esac
    echo "=========================================================="
    echo "== Building for $id ($img)"
    echo "=========================================================="
    "$RUNTIME" run --rm \
        -e CARGO_TARGET_DIR="/work/target-$id" \
        -e INSTALL_CMD="$install" \
        -e DIST="$id" \
        -e CARGO_GENERATE_RPM_VERSION="$CARGO_GENERATE_RPM_VERSION" \
        -v "$PWD:/work:Z" \
        -w /work \
        "$img" \
        bash -euxc '
            eval "$INSTALL_CMD"
            curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | \
                sh -s -- -y --default-toolchain stable --profile minimal
            source "$HOME/.cargo/env"
            cargo install cargo-generate-rpm --locked --version "$CARGO_GENERATE_RPM_VERSION"
            make man
            cargo build --release
            make completions
            cargo generate-rpm --target-dir "$CARGO_TARGET_DIR" \
                --set-metadata "release = \"1.$DIST\""
        '
    SRC_RPM=$(ls "target-$id/generate-rpm/"*.rpm | head -1)
    cp "$SRC_RPM" dist/
    echo "-> dist/$(basename "$SRC_RPM")"
done

echo
echo "Built packages:"
ls -la dist/
