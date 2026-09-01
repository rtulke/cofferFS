#!/usr/bin/env bash
# Installs each distro's own .deb (from dist/, see build-deb.sh) into a fresh
# container of that same distro, via a real `apt-get install` (so declared
# Depends: actually have to resolve, not just `dpkg -i --force-depends`),
# then runs a full create/mount/write/read/umount/check cycle - not just
# "did it install", but does the mounted filesystem actually work.
set -euo pipefail
cd "$(dirname "$0")/.."   # repo root

RUNTIME=docker
command -v docker >/dev/null 2>&1 || RUNTIME=podman

declare -A TARGET_IMAGES=(
    [debian12]=debian:12-slim
    [debian13]=debian:13-slim
    [ubuntu2404]=ubuntu:24.04
    [ubuntu2604]=ubuntu:26.04
)

for id in "${!TARGET_IMAGES[@]}"; do
    img="${TARGET_IMAGES[$id]}"
    DEB=$(ls dist/coffer_*_"${id}"_amd64.deb 2>/dev/null | head -1)
    if [ -z "$DEB" ]; then
        echo "no .deb for $id in dist/ - run packaging/build-deb.sh first" >&2
        exit 1
    fi

    echo "=========================================================="
    echo "== $id ($img) <- $DEB"
    echo "=========================================================="
    "$RUNTIME" run --rm \
        --cap-add SYS_ADMIN --device /dev/fuse \
        --security-opt seccomp=unconfined --security-opt apparmor=unconfined \
        -v "$PWD/$DEB:/tmp/coffer.deb:Z,ro" \
        "$img" \
        bash -euxc '
            apt-get update -qq
            apt-get install -y --no-install-recommends /tmp/coffer.deb

            coffer --help
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
