#!/usr/bin/env bash
# Installs each distro's own package (from dist/, see build-deb.sh and
# build-rpm.sh) into a fresh container of that same distro, via a real
# `apt-get install` / `dnf install` / `zypper install` (so declared
# dependencies actually have to resolve, not just `dpkg -i --force-depends`
# or `rpm -i --nodeps`), then runs a full create/mount/write/read/umount/
# check cycle - not just "did it install", but does the mounted filesystem
# actually work.
set -euo pipefail
cd "$(dirname "$0")/.."   # repo root

RUNTIME=docker
command -v docker >/dev/null 2>&1 || RUNTIME=podman

# id -> image. The .deb ids match build-deb.sh, the .rpm ids build-rpm.sh;
# which format a target uses is decided by its image below.
TARGET_IDS=(debian12 debian13 ubuntu2404 ubuntu2604 fedora43 fedora44 el9 el10 leap160 tumbleweed)
declare -A TARGET_IMAGES=(
    [debian12]=debian:12-slim
    [debian13]=debian:13-slim
    [ubuntu2404]=ubuntu:24.04
    [ubuntu2604]=ubuntu:26.04
    [fedora43]=fedora:43
    [fedora44]=fedora:44
    [el9]=almalinux:9
    [el10]=almalinux:10
    [leap160]=opensuse/leap:16.0
    [tumbleweed]=opensuse/tumbleweed
)

# The package-manager half of the smoke test, per family. The functional
# half (create, mount, write, read, umount, check) is shared below.
#
# apt: a plain local-path install. dnf: the same, local .rpm files resolve
# their Requires: against the enabled repos. zypper: additionally needs
# --allow-unsigned-rpm since the packages aren't GPG-signed. Fedora/EL/
# openSUSE container images set tsflags=nodocs (or the zypper equivalent),
# so %doc files like the man page are legitimately not on disk there -
# check those via the package manifest, not the filesystem.
APT_INSTALL='apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends /tmp/pkg/*.deb
             dpkg -L coffer | grep -qx /usr/share/man/man1/coffer.1.gz'
DNF_INSTALL='dnf install -y /tmp/pkg/*.rpm
             rpm -ql coffer | grep -qx /usr/share/man/man1/coffer.1.gz'
ZYPPER_INSTALL='zypper --non-interactive --gpg-auto-import-keys refresh
             zypper --non-interactive install --allow-unsigned-rpm /tmp/pkg/*.rpm
             rpm -ql coffer | grep -qx /usr/share/man/man1/coffer.1.gz'

for id in "${TARGET_IDS[@]}"; do
    img="${TARGET_IMAGES[$id]}"
    case "$img" in
        debian:*|ubuntu:*) install="$APT_INSTALL";    PKG=$(ls dist/coffer_*_"${id}"_amd64.deb 2>/dev/null | head -1) ;;
        opensuse/*)        install="$ZYPPER_INSTALL"; PKG=$(ls dist/coffer-*."${id}".x86_64.rpm 2>/dev/null | head -1) ;;
        *)                 install="$DNF_INSTALL";    PKG=$(ls dist/coffer-*."${id}".x86_64.rpm 2>/dev/null | head -1) ;;
    esac
    if [ -z "$PKG" ]; then
        echo "no package for $id in dist/ - run packaging/build-deb.sh / build-rpm.sh first" >&2
        exit 1
    fi

    echo "=========================================================="
    echo "== $id ($img) <- $PKG"
    echo "=========================================================="
    "$RUNTIME" run --rm \
        --cap-add SYS_ADMIN --device /dev/fuse \
        --security-opt seccomp=unconfined --security-opt apparmor=unconfined \
        -v "$PWD/$PKG:/tmp/pkg/$(basename "$PKG"):Z,ro" \
        -e INSTALL_CMD="$install" \
        "$img" \
        bash -euxc '
            eval "$INSTALL_CMD"

            coffer --help >/dev/null
            coffer --version
            test -f /usr/share/bash-completion/completions/coffer

            mkdir -p /root/mnt
            printf "testpass\ntestpass\n" | coffer create /root/vault.coffer
            printf "testpass\n" | coffer mount /root/vault.coffer /root/mnt --foreground &
            MOUNT_PID=$!
            sleep 1
            echo "hello from container test" > /root/mnt/f.txt
            cat /root/mnt/f.txt
            grep -q "hello from container test" /root/mnt/f.txt

            fusermount3 -u /root/mnt
            wait "$MOUNT_PID" 2>/dev/null || true

            printf "testpass\n" | coffer check /root/vault.coffer

            echo "OK: install + create + mount + write + read + umount + check all worked"
        '
    echo "== $id: PASS"
    echo
done

echo "All targets passed."
