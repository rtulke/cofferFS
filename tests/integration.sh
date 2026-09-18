#!/usr/bin/env bash
# End-to-end suite against a real mount. Everything a user could do to a
# container is done here for real: create, mount (daemonized), write, read,
# rename, truncate, sparse files, symlinks, rsync round-trips, unmount and
# remount for persistence, read-only mounts, the idle unmount, a live
# backup, a hard kill of the mount daemon mid-write with a check
# afterwards, passwd, compact, the registry, --password-command.
#
#     tests/integration.sh                      # target/release/coffer
#     COFFER=/usr/bin/coffer tests/integration.sh
#
# Linux only (FUSE). Needs fusermount3, rsync, flock, sha256sum, truncate,
# getfattr/setfattr (package attr) and python3. Set COFFER_PREV to an older
# release's binary to also run the cross-version compatibility section.
# Uses its own registry (COFFER_CONFIG) and a temp directory; never touches
# ~/.coffer. Exit status is the number of failed checks.
set -uo pipefail

COFFER=$(realpath "${COFFER:-$(dirname "$0")/../target/release/coffer}")
WORK=$(mktemp -d "${TMPDIR:-/tmp}/coffer-it.XXXXXX")
export COFFER_CONFIG="$WORK/registry"
PW="$WORK/pw"; printf 'integration-test-password\n' > "$PW"; chmod 600 "$PW"
PW2="$WORK/pw2"; printf 'second-password\n' > "$PW2"; chmod 600 "$PW2"
V="$WORK/v.coffer"
MNT="$WORK/mnt"
LOG="$WORK/mount.log"
pass=0; fail=0

section() { echo; echo "== $1"; }
ok()      { pass=$((pass+1)); echo "  ok   $1"; }
bad()     { fail=$((fail+1)); echo "  FAIL $1" >&2; }
# check DESCRIPTION CMD...   - the command's success is the check
check()       { local d=$1; shift; if "$@" >/dev/null 2>&1; then ok "$d"; else bad "$d"; fi; }
# expect_fail DESCRIPTION CMD... - the command must fail
expect_fail() { local d=$1; shift; if "$@" >/dev/null 2>&1; then bad "$d (succeeded, but should have failed)"; else ok "$d"; fi; }
# eq DESCRIPTION EXPECTED ACTUAL
eq()          { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (expected '$2', got '$3')"; fi; }

is_mounted()    { awk -v m="$1" '$5 == m && / - fuse coffer /' /proc/self/mountinfo | grep -q .; }
wait_mounted()  { for _ in $(seq 1 50); do is_mounted "$1" && return 0; sleep 0.2; done; return 1; }
wait_unmounted(){ for _ in $(seq 1 "${2:-50}"); do is_mounted "$1" || return 0; sleep 0.2; done; return 1; }
# The daemon holds an exclusive flock on the container; once it is gone the
# lock is free. This is the "did the daemon really exit" check.
lock_free()     { flock -n "$1" true; }
wait_lock_free(){ for _ in $(seq 1 50); do lock_free "$1" && return 0; sleep 0.2; done; return 1; }
daemon_pid()    { pgrep -f "coffer mount $1 " | head -1; }
# A few checks run inside `bash -c` (to chain commands); functions and the
# variables they use have to be exported to be visible there.
export -f is_mounted wait_mounted wait_unmounted lock_free wait_lock_free
export COFFER MNT V WORK

mnt() {   # mnt CONTAINER MOUNTPOINT [extra mount flags...]
    local c=$1 m=$2; shift 2
    mkdir -p "$m"
    "$COFFER" mount "$c" "$m" --password-file "$PW" --log "$LOG" "$@" >/dev/null && wait_mounted "$m"
}
umnt() {  # umnt CONTAINER MOUNTPOINT
    "$COFFER" umount "$2" >/dev/null && wait_unmounted "$2" && wait_lock_free "$1"
}

cleanup() {
    for m in "$MNT" "$WORK/mnt2" "$WORK/mnt3" "$WORK/mnt-old"; do
        is_mounted "$m" 2>/dev/null && fusermount3 -u -z "$m" 2>/dev/null
    done
    sleep 0.3
    pkill -f "coffer mount $WORK/" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

echo "coffer: $("$COFFER" --version)"
echo "work:   $WORK"

# ---------------------------------------------------------------------------
section "create"
check "create"                          "$COFFER" create "$V" --password-file "$PW"
expect_fail "create refuses existing"   "$COFFER" create "$V" --password-file "$PW"
check "info with right password"        "$COFFER" info "$V" --password-file "$PW"
expect_fail "info with wrong password"  "$COFFER" info "$V" --password-file "$PW2"
check "check on fresh container"        "$COFFER" check "$V" --password-file "$PW"
eq "container mode 0600" "600" "$(stat -c %a "$V")"

# ---------------------------------------------------------------------------
section "basic filesystem operations"
check "mount (daemonized)" mnt "$V" "$MNT"
check "mkdir -p nested"    mkdir -p "$MNT/a/b/c"
check "write file"         bash -c "echo hello > '$MNT/a/b/c/f.txt'"
eq "read back" "hello" "$(cat "$MNT/a/b/c/f.txt")"
check "append"             bash -c "echo world >> '$MNT/a/b/c/f.txt'"
eq "append read back" "2" "$(wc -l < "$MNT/a/b/c/f.txt")"
check "rename file"        mv "$MNT/a/b/c/f.txt" "$MNT/a/b/c/g.txt"
expect_fail "old name gone" test -e "$MNT/a/b/c/f.txt"
check "rename dir"         mv "$MNT/a/b" "$MNT/a/bb"
eq "file under renamed dir" "hello" "$(head -1 "$MNT/a/bb/c/g.txt")"
check "rename over existing" bash -c "echo x > '$MNT/x'; echo y > '$MNT/y'; mv '$MNT/x' '$MNT/y'"
eq "rename over existing content" "x" "$(cat "$MNT/y")"
check "dirs for rename tests" bash -c "mkdir -p '$MNT/rd1' '$MNT/rd2' '$MNT/rd3' && echo f > '$MNT/rd2/f'"
expect_fail "rename dir over non-empty dir is ENOTEMPTY" mv -T "$MNT/rd1" "$MNT/rd2"
eq "non-empty target dir survived" "f" "$(cat "$MNT/rd2/f")"
check "source dir survived too" test -d "$MNT/rd1"
check "rename dir over empty dir"  mv -T "$MNT/rd1" "$MNT/rd3"
expect_fail "old dir name gone" test -e "$MNT/rd1"
check "renamed dir usable" bash -c "echo g > '$MNT/rd3/g' && rm -r '$MNT/rd3' '$MNT/rd2'"
# The kernel caches attributes for one second (fs.rs TTL); the mode change
# from the write is visible after that, so the check waits it out.
check "setuid bit is dropped by a write" bash -c "echo s > '$MNT/suid' && chmod 4755 '$MNT/suid' && [ \"\$(stat -c %a '$MNT/suid')\" = 4755 ] && echo more >> '$MNT/suid' && sleep 1.5 && [ \"\$(stat -c %a '$MNT/suid')\" = 755 ]"
check "symlink"            ln -s a/bb/c/g.txt "$MNT/link"
eq "readlink" "a/bb/c/g.txt" "$(readlink "$MNT/link")"
eq "read through symlink" "hello" "$(head -1 "$MNT/link")"
check "truncate shrink"    truncate -s 3 "$MNT/a/bb/c/g.txt"
eq "size after shrink" "3" "$(stat -c %s "$MNT/a/bb/c/g.txt")"
eq "content after shrink" "hel" "$(cat "$MNT/a/bb/c/g.txt")"
check "truncate grow"      truncate -s 10 "$MNT/a/bb/c/g.txt"
eq "size after grow" "10" "$(stat -c %s "$MNT/a/bb/c/g.txt")"
eq "grow filled with zeros" "0" "$(tail -c 7 "$MNT/a/bb/c/g.txt" | tr -d '\0' | wc -c | tr -d ' ')"
check "chmod"              chmod 640 "$MNT/a/bb/c/g.txt"
eq "mode reported" "640" "$(stat -c %a "$MNT/a/bb/c/g.txt")"
check "touch -d"           touch -d '2020-01-02 03:04:05 UTC' "$MNT/a/bb/c/g.txt"
eq "mtime reported" "1577934245" "$(stat -c %Y "$MNT/a/bb/c/g.txt")"
check "rm file"            rm "$MNT/y"
expect_fail "rmdir non-empty" rmdir "$MNT/a"
check "rm -r tree"         rm -r "$MNT/a"
expect_fail "tree gone"    test -e "$MNT/a"
check "rmdir empty"        bash -c "mkdir '$MNT/e' && rmdir '$MNT/e'"
check "statfs answers"     df "$MNT"

# ---------------------------------------------------------------------------
section "data integrity"
head -c 20971520 /dev/urandom > "$WORK/big.bin"
BIG_SUM=$(sha256sum "$WORK/big.bin" | cut -d' ' -f1)
check "write 20 MB"        cp "$WORK/big.bin" "$MNT/big.bin"
eq "20 MB read back identical" "$BIG_SUM" "$(sha256sum "$MNT/big.bin" | cut -d' ' -f1)"
check "sparse write at 50 MB" dd if=/dev/urandom of="$MNT/sparse" bs=1M seek=50 count=1 status=none
eq "sparse size" "53477376" "$(stat -c %s "$MNT/sparse")"
eq "hole reads as zeros" "0" "$(head -c 1048576 "$MNT/sparse" | tr -d '\0' | wc -c | tr -d ' ')"
check "overwrite middle of file" dd if=/dev/zero of="$MNT/big.bin" bs=4096 seek=1000 count=3 conv=notrunc status=none
eq "size unchanged after overwrite" "20971520" "$(stat -c %s "$MNT/big.bin")"
eq "overwritten range is zeros" "0" "$(dd if="$MNT/big.bin" bs=4096 skip=1000 count=3 status=none | tr -d '\0' | wc -c | tr -d ' ')"
check "500 small files"    bash -c "mkdir '$MNT/many' && for i in \$(seq 1 500); do echo \$i > '$MNT/many/f'\$i; done"
eq "find counts 500" "500" "$(find "$MNT/many" -type f | wc -l)"
eq "file 250 content" "250" "$(cat "$MNT/many/f250")"
eq "ls -f counts 500 (readdir)" "500" "$(ls -f "$MNT/many" | grep -c '^f')"
check "empty file"         touch "$MNT/empty"
eq "empty file size" "0" "$(stat -c %s "$MNT/empty")"

# ---------------------------------------------------------------------------
section "rsync round-trip"
mkdir -p "$WORK/src/d1/d2" "$WORK/src/d3"
for i in 0 1 100 4095 4096 4097 131071 131072 131073 1000000; do head -c $i /dev/urandom > "$WORK/src/d1/s$i"; done
head -c 3000000 /dev/urandom > "$WORK/src/d3/three-mb"
ln -s d1/s100 "$WORK/src/link"
chmod 600 "$WORK/src/d3/three-mb"
check "rsync -a into mount" rsync -a "$WORK/src/" "$MNT/tree/"
check "diff -r src vs mount" diff -r "$WORK/src" "$MNT/tree"
check "rsync -a out of mount" rsync -a "$MNT/tree/" "$WORK/out/"
check "diff -r src vs out"   diff -r "$WORK/src" "$WORK/out"
eq "mode survived rsync" "600" "$(stat -c %a "$MNT/tree/d3/three-mb")"
eq "symlink survived rsync" "d1/s100" "$(readlink "$MNT/tree/link")"

# ---------------------------------------------------------------------------
section "persistence across unmount / remount"
check "umount"             umnt "$V" "$MNT"
check "container check"    "$COFFER" check "$V" --password-file "$PW"
check "remount"            mnt "$V" "$MNT"
# big.bin had a range zeroed above; apply the same to the reference copy first.
dd if=/dev/zero of="$WORK/big.bin" bs=4096 seek=1000 count=3 conv=notrunc status=none
eq "20 MB (with zeroed range) identical after remount" "$(sha256sum "$WORK/big.bin" | cut -d' ' -f1)" "$(sha256sum "$MNT/big.bin" | cut -d' ' -f1)"
check "tree identical after remount" diff -r "$WORK/src" "$MNT/tree"
eq "500 files still there" "500" "$(find "$MNT/many" -type f | wc -l)"
eq "sparse size persisted" "53477376" "$(stat -c %s "$MNT/sparse")"
check "umount again"       umnt "$V" "$MNT"

# ---------------------------------------------------------------------------
section "live backup and info"
check "mount"              mnt "$V" "$MNT"
check "backup while mounted" "$COFFER" backup "$V" "$WORK/backup.coffer" --password-file "$PW"
check "backup passes check"  "$COFFER" check "$WORK/backup.coffer" --password-file "$PW"
check "mount backup read-only elsewhere" mnt "$WORK/backup.coffer" "$WORK/mnt2" --read-only
check "backup has the tree"  diff -r "$WORK/src" "$WORK/mnt2/tree"
check "umount backup"        umnt "$WORK/backup.coffer" "$WORK/mnt2"
check "umount"               umnt "$V" "$MNT"
INFO=$("$COFFER" info "$V" --password-file "$PW")
eq "info counts files" "1" "$(echo "$INFO" | grep -c '^Files:')"

# ---------------------------------------------------------------------------
section "read-only mount"
check "mount -r"           mnt "$V" "$MNT" -r
check "mountinfo says ro"  bash -c "awk -v m='$MNT' '\$5 == m {print \$6}' /proc/self/mountinfo | grep -q '^ro,'"
expect_fail "write refused"  bash -c "echo x > '$MNT/nope'"
expect_fail "mkdir refused"  mkdir "$MNT/nope"
expect_fail "rm refused"     rm "$MNT/empty"
expect_fail "truncate refused" truncate -s 1 "$MNT/sparse"
eq "read still works" "250" "$(cat "$MNT/many/f250")"
check "umount ro"          umnt "$V" "$MNT"
expect_fail "--compact-on-idle with --read-only refused" "$COFFER" mount "$V" "$MNT" -r --compact-on-idle 1h --password-file "$PW"
check "--ro alias"         mnt "$V" "$MNT" --ro
check "umount"             umnt "$V" "$MNT"

# ---------------------------------------------------------------------------
section "idle timeout"
check "mount --idle-timeout 2s" mnt "$V" "$MNT" --idle-timeout 2s
check "auto-unmounted within 40 s" wait_unmounted "$MNT" 200
check "daemon released the lock" wait_lock_free "$V"
check "log mentions the idle unmount" grep -q "auto-unmounting" "$LOG"

# ---------------------------------------------------------------------------
section "crash: kill -9 the daemon mid-write"
check "mount"              mnt "$V" "$MNT"
PID=$(daemon_pid "$V")
check "daemon pid found"   test -n "$PID"
( dd if=/dev/urandom of="$MNT/victim" bs=1M count=300 status=none 2>/dev/null ) &
WRITER=$!
sleep 0.7
check "kill -9 daemon"     kill -9 "$PID"
wait "$WRITER" 2>/dev/null || true
fusermount3 -u -z "$MNT" 2>/dev/null || true
check "mountpoint released" wait_unmounted "$MNT"
check "lock released after kill" wait_lock_free "$V"
check "check passes after crash" "$COFFER" check "$V" --password-file "$PW"
check "remount after crash" mnt "$V" "$MNT"
check "tree intact after crash" diff -r "$WORK/src" "$MNT/tree"
eq "500 files intact after crash" "500" "$(find "$MNT/many" -type f | wc -l)"
check "umount"             umnt "$V" "$MNT"

# ---------------------------------------------------------------------------
section "passwd, compact, max-size"
check "passwd"             "$COFFER" passwd "$V" --password-file "$PW" --new-password-file "$PW2"
expect_fail "old password refused" "$COFFER" info "$V" --password-file "$PW"
check "new password works" "$COFFER" info "$V" --password-file "$PW2"
check "passwd back"        "$COFFER" passwd "$V" --password-file "$PW2" --new-password-file "$PW"
BEFORE=$(stat -c %s "$V")
check "mount"              mnt "$V" "$MNT"
check "delete the big files" rm -f "$MNT/big.bin" "$MNT/sparse" "$MNT/victim" "$MNT/tree/d3/three-mb"
check "umount"             umnt "$V" "$MNT"
check "compact"            "$COFFER" compact "$V" --password-file "$PW"
AFTER=$(stat -c %s "$V")
check "compact shrank the file ($BEFORE -> $AFTER)" test "$AFTER" -lt "$BEFORE"
check "check after compact" "$COFFER" check "$V" --password-file "$PW"
expect_fail "compact refused while mounted" bash -c "'$COFFER' mount '$V' '$MNT' --password-file '$PW' >/dev/null && sleep 0.5 && '$COFFER' compact '$V' --password-file '$PW'"
umnt "$V" "$MNT" >/dev/null 2>&1 || true
V3="$WORK/capped.coffer"
check "create --max-size 3M" "$COFFER" create "$V3" --max-size 3M --password-file "$PW"
check "mount capped"       mnt "$V3" "$WORK/mnt3"
expect_fail "write past the ceiling fails" dd if=/dev/zero of="$WORK/mnt3/fill" bs=1M count=6 status=none
check "umount capped"      umnt "$V3" "$WORK/mnt3"
check "capped container still checks" "$COFFER" check "$V3" --password-file "$PW"

# ---------------------------------------------------------------------------
section "registry and password sources"
check "add alias"          "$COFFER" add it "$V" "$MNT" --password-file "$PW" --log-file "$LOG"
check "list shows alias"   bash -c "'$COFFER' list | grep -q '^it '"
check "mount by alias"     bash -c "'$COFFER' mount it >/dev/null && wait_mounted '$MNT'"
check "list shows mounted" bash -c "'$COFFER' list | grep '^it ' | grep -q mounted"
check "umount by alias"    bash -c "'$COFFER' umount it >/dev/null && wait_unmounted '$MNT' && wait_lock_free '$V'"
check "add --read-only alias" "$COFFER" add ro "$V" "$MNT" --password-file "$PW" --read-only
check "mount ro alias"     bash -c "'$COFFER' mount ro >/dev/null && wait_mounted '$MNT'"
expect_fail "ro alias refuses write" bash -c "echo x > '$MNT/nope'"
check "umount ro alias"    bash -c "'$COFFER' umount ro >/dev/null && wait_unmounted '$MNT' && wait_lock_free '$V'"
check "remove alias"       "$COFFER" remove ro
expect_fail "removed alias gone" bash -c "'$COFFER' list | grep -q '^ro '"
check "--password-command" "$COFFER" info "$V" --password-command "cat $PW"
expect_fail "--password-command failing command" "$COFFER" info "$V" --password-command false
expect_fail "--password-command empty output" "$COFFER" info "$V" --password-command true
check "add --password-command alias" "$COFFER" add pc "$V" "$MNT" --password-command "cat $PW"
check "mount via password_command" bash -c "'$COFFER' mount pc >/dev/null && wait_mounted '$MNT'"
check "umount"             bash -c "'$COFFER' umount pc >/dev/null && wait_unmounted '$MNT' && wait_lock_free '$V'"

# ---------------------------------------------------------------------------
section "extended attributes"
check "mount"              mnt "$V" "$MNT"
check "file for xattrs"    bash -c "echo hi > '$MNT/xf'"
check "setfattr user.color" setfattr -n user.color -v blue "$MNT/xf"
eq "getfattr value" "blue" "$(getfattr -n user.color --only-values "$MNT/xf" 2>/dev/null)"
check "second attribute"   setfattr -n user.size -v big "$MNT/xf"
eq "listxattr shows both" "2" "$(getfattr -d "$MNT/xf" 2>/dev/null | grep -c '^user\.')"
check "removexattr"        setfattr -x user.size "$MNT/xf"
eq "listxattr shows one"  "1" "$(getfattr -d "$MNT/xf" 2>/dev/null | grep -c '^user\.')"
expect_fail "getxattr of removed name" getfattr -n user.size "$MNT/xf"
expect_fail "removexattr of missing name" setfattr -x user.size "$MNT/xf"
check "xattr on a directory" bash -c "mkdir '$MNT/xd' && setfattr -n user.dir -v yes '$MNT/xd'"
eq "directory xattr value" "yes" "$(getfattr -n user.dir --only-values "$MNT/xd" 2>/dev/null)"
check "setxattr flags and limits (python)" python3 - "$MNT/xf" <<'PY'
import os, sys
p = sys.argv[1]
try:
    os.setxattr(p, "user.color", b"x", os.XATTR_CREATE); sys.exit("XATTR_CREATE on existing name succeeded")
except FileExistsError:
    pass
try:
    os.setxattr(p, "user.nope", b"x", os.XATTR_REPLACE); sys.exit("XATTR_REPLACE on missing name succeeded")
except OSError as e:
    assert e.errno == 61, e     # ENODATA
try:
    os.setxattr(p, "user.big", b"x" * 70000); sys.exit("70000-byte value accepted")
except OSError as e:
    assert e.errno in (7, 34), e   # E2BIG / ERANGE
os.setxattr(p, "user.max", b"y" * 65536)
assert len(os.getxattr(p, "user.max")) == 65536
os.setxattr(p, "user.bin", bytes(range(256)))
assert os.getxattr(p, "user.bin") == bytes(range(256))
assert sorted(os.listxattr(p)) == ["user.bin", "user.color", "user.max"], os.listxattr(p)
PY
check "cp --preserve=xattr" cp --preserve=xattr "$MNT/xf" "$MNT/xf2"
eq "copied xattr value" "blue" "$(getfattr -n user.color --only-values "$MNT/xf2" 2>/dev/null)"
check "rsync -X into mount"  bash -c "mkdir -p '$WORK/xsrc' && echo a > '$WORK/xsrc/a' && setfattr -n user.rs -v 1 '$WORK/xsrc/a' && rsync -aX '$WORK/xsrc/' '$MNT/xtree/'"
eq "rsync -X preserved xattr" "1" "$(getfattr -n user.rs --only-values "$MNT/xtree/a" 2>/dev/null)"
check "rename keeps xattrs" mv "$MNT/xf2" "$MNT/xf3"
eq "xattr after rename" "blue" "$(getfattr -n user.color --only-values "$MNT/xf3" 2>/dev/null)"
check "rename over a file with xattrs" bash -c "echo z > '$MNT/plain' && mv '$MNT/plain' '$MNT/xf3'"
expect_fail "overwritten file's xattrs are gone" getfattr -n user.color "$MNT/xf3"
check "unlink a file with xattrs" rm "$MNT/xf3"
check "umount"             umnt "$V" "$MNT"
check "remount"            mnt "$V" "$MNT"
eq "xattr survives remount" "blue" "$(getfattr -n user.color --only-values "$MNT/xf" 2>/dev/null)"
eq "directory xattr survives remount" "yes" "$(getfattr -n user.dir --only-values "$MNT/xd" 2>/dev/null)"
check "umount"             umnt "$V" "$MNT"
check "mount read-only"    mnt "$V" "$MNT" -r
eq "getxattr on read-only mount" "blue" "$(getfattr -n user.color --only-values "$MNT/xf" 2>/dev/null)"
expect_fail "setxattr refused on read-only mount" setfattr -n user.x -v y "$MNT/xf"
expect_fail "removexattr refused on read-only mount" setfattr -x user.color "$MNT/xf"
check "umount"             umnt "$V" "$MNT"
check "check after xattrs" "$COFFER" check "$V" --password-file "$PW"

# ---------------------------------------------------------------------------
# Cross-version compatibility, when a previous release is available
# (COFFER_PREV=/path/to/older/coffer): containers must open in both
# directions, and what the newer version adds (the xattrs table) must not
# get in the older one's way.
if [ -n "${COFFER_PREV:-}" ] && [ -x "$COFFER_PREV" ]; then
    section "compatibility with $("$COFFER_PREV" --version)"
    OLDV="$WORK/old.coffer"; OLDM="$WORK/mnt-old"; mkdir -p "$OLDM"
    check "previous version creates a container" "$COFFER_PREV" create "$OLDV" --password-file "$PW"
    check "this version mounts it"    mnt "$OLDV" "$OLDM"
    check "this version writes to it" bash -c "echo new > '$OLDM/from-new' && setfattr -n user.k -v v '$OLDM/from-new'"
    check "umount"                    umnt "$OLDV" "$OLDM"
    check "previous version checks it after our writes" "$COFFER_PREV" check "$OLDV" --password-file "$PW"
    check "previous version mounts it" bash -c "'$COFFER_PREV' mount '$OLDV' '$OLDM' --password-file '$PW' >/dev/null && wait_mounted '$OLDM'"
    eq "previous version reads our file" "new" "$(cat "$OLDM/from-new")"
    check "previous version writes"   bash -c "echo old > '$OLDM/from-old'"
    check "previous version deletes the file with our xattr" rm "$OLDM/from-new"
    check "umount (previous version)" bash -c "'$COFFER_PREV' umount '$OLDM' >/dev/null && wait_unmounted '$OLDM' && wait_lock_free '$OLDV'"
    check "this version mounts it again" mnt "$OLDV" "$OLDM"
    expect_fail "the deleted file stays deleted" test -e "$OLDM/from-new"
    check "re-create the same name: no stale xattr" bash -c "echo again > '$OLDM/from-new'"
    expect_fail "no xattr resurfaces on the new file" getfattr -n user.k "$OLDM/from-new"
    eq "previous version's file is there" "old" "$(cat "$OLDM/from-old")"
    check "umount"                    umnt "$OLDV" "$OLDM"
    check "previous version mounts this version's container" bash -c "'$COFFER_PREV' mount '$V' '$OLDM' --password-file '$PW' >/dev/null && wait_mounted '$OLDM'"
    eq "previous version reads it" "250" "$(cat "$OLDM/many/f250")"
    check "umount (previous version)" bash -c "'$COFFER_PREV' umount '$OLDM' >/dev/null && wait_unmounted '$OLDM' && wait_lock_free '$V'"
    check "previous version checks this version's container" "$COFFER_PREV" check "$V" --password-file "$PW"
    check "read-only mount of a container without the xattrs table" bash -c "'$COFFER_PREV' create '$WORK/old2.coffer' --password-file '$PW' >/dev/null && '$COFFER' mount '$WORK/old2.coffer' '$OLDM' -r --password-file '$PW' --log '$LOG' >/dev/null && wait_mounted '$OLDM'"
    check "listxattr answers (empty) there" getfattr -d "$OLDM"
    check "getxattr there is ENODATA, not EIO (python)" python3 - "$OLDM" <<'PY'
import os, sys
p = sys.argv[1]
assert os.listxattr(p) == [], os.listxattr(p)
try:
    os.getxattr(p, "user.nope"); sys.exit("getxattr succeeded on a container without the table")
except OSError as e:
    assert e.errno == 61, e     # ENODATA, the "no such attribute" answer
PY
    check "umount"                    umnt "$WORK/old2.coffer" "$OLDM"
else
    echo; echo "== compatibility: skipped (set COFFER_PREV to a previous release's binary)"
fi

# ---------------------------------------------------------------------------
echo
echo "passed: $pass  failed: $fail"
exit "$fail"
