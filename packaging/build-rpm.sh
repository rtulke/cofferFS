#!/usr/bin/env bash
# Builds one .rpm per target distro, natively in that distro's own container,
# via cargo-generate-rpm. The RPM counterpart of build-deb.sh.
#
# Why one per distro and not a single "el9-built runs everywhere" package:
# the same libfuse3 SONAME split as on the Debian side. EL9/EL10 and
# Fedora 43 ship fuse 3.10-3.16 (libfuse3.so.3), while Fedora 44 and
# Tumbleweed are on 3.18 (libfuse3.so.4), and a binary linked against one
# can't load the other. Building natively per target means each package's
# automatically discovered `Requires:` simply names whatever that distro
# actually ships. SQLCipher/OpenSSL are statically bundled (see Cargo.toml),
# so libfuse3 is the only runtime library dependency that varies.
#
# The dist tag goes into the RPM's Release field (`1.fc44`, `1.el9`, ...),
# the way distro packages themselves are named, so the file names come out
# as coffer-<version>-1.<dist>.x86_64.rpm without any renaming step.
set -euo pipefail
cd "$(dirname "$0")/.."   # repo root

RUNTIME=docker
command -v docker >/dev/null 2>&1 || RUNTIME=podman

CARGO_GENERATE_RPM_VERSION=0.21.0

# id -> image, and id -> dist tag. Kept as two ordered lists (not an
# associative array) so the build order is deterministic.
TARGET_IDS=(fedora43 fedora44 el9 el10 leap160 tumbleweed)
declare -A TARGET_IMAGES=(
    [fedora43]=fedora:43
    [fedora44]=fedora:44
    [el9]=almalinux:9
    [el10]=almalinux:10
    [leap160]=opensuse/leap:16.0
    [tumbleweed]=opensuse/tumbleweed
)
declare -A TARGET_DIST=(
    [fedora43]=fc43
    [fedora44]=fc44
    [el9]=el9
    [el10]=el10
    [leap160]=lp160
    [tumbleweed]=tw
)

# Per package-manager family. rpm-build is installed so cargo-generate-rpm's
# default `auto-req = "auto"` finds the distro's own /usr/lib/rpm/find-requires
# and emits exactly the versioned `Requires:` that distro's packages carry,
# instead of falling back to its ldd-based approximation. fuse3-devel sits in
# AppStream on EL9/EL10 (no CRB needed) and in the main repo elsewhere.
DNF_INSTALL='dnf install -y --setopt=install_weak_deps=False \
    gcc make pkgconf-pkg-config perl fuse3-devel fuse3 rpm-build git curl ca-certificates tar gzip'
ZYPPER_INSTALL='zypper --non-interactive --gpg-auto-import-keys refresh && \
    zypper --non-interactive install --no-recommends \
    gcc make pkg-config perl fuse3-devel fuse3 rpm-build git curl ca-certificates tar gzip'

mkdir -p dist
rm -f dist/coffer-*.rpm

for id in "${TARGET_IDS[@]}"; do
    img="${TARGET_IMAGES[$id]}"
    dist="${TARGET_DIST[$id]}"
    case "$img" in
        opensuse/*) install="$ZYPPER_INSTALL" ;;
        *)          install="$DNF_INSTALL" ;;
    esac
    echo "=========================================================="
    echo "== Building for $id ($img, dist tag .$dist)"
    echo "=========================================================="
    "$RUNTIME" run --rm \
        -e CARGO_TARGET_DIR="/work/target-$id" \
        -e INSTALL_CMD="$install" \
        -e DIST="$dist" \
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
