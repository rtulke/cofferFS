#!/usr/bin/env bash
# End-to-end suite against a real mount. Everything a user could do to a
# container is done here for real: create, mount (daemonized), write, read,
# rename (including the renameat2 flags), truncate, sparse files,
# symlinks, extended attributes, the setuid/setgid drop on modification,
# rsync round-trips, unmount and remount for persistence, read-only
# mounts, the idle unmount, a live backup, a hard kill of the mount daemon
# mid-write with a check afterwards, passwd, compact, the registry and
# --password-command. With COFFER_PREV set, also a cross-version round
# trip against that release.
#
#     tests/integration.sh                      # target/release/coffer
#     COFFER=/usr/bin/coffer tests/integration.sh
#
# Linux only (FUSE). Needs fusermount3, rsync, flock, sha256sum, truncate, script,
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
# The kernel caches attributes (fs.rs TTL), so a mode change made by the
# filesystem itself - dropping setuid on write, say - becomes visible to
# stat only once that cache expires. Poll instead of hardcoding the TTL.
wait_mode()     { for _ in $(seq 1 40); do [ "$(stat -c %a "$1")" = "$2" ] && return 0; sleep 0.25; done; return 1; }
export -f wait_mode
# Fills a directory with $2 files named f0000.. created in exactly the
# reverse of their sorted order, so inode order is the opposite of name
# order and a readdir that returned either one can be told from the other.
# Reversed rather than shuffled on purpose: it is the sharpest case, and
# it is the same every run (`shuf --random-source=/dev/zero` is not a
# shuffle at all - it returns the input unchanged).
fill_dir()      { mkdir -p "$1" || return 1; seq $(( $2 - 1 )) -1 0 \
                      | while read -r i; do printf 'f%04d\n' "$i"; done \
                      | while read -r n; do : > "$1/$n" || return 1; done; }
link_many()     { : > "$1" || return 1; for i in $(seq 1 "$2"); do ln "$1" "$1-$i" || return 1; done; }
export -f fill_dir link_many
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
# renameat2(2) flags: `mv -n` asks for RENAME_NOREPLACE and must not
# overwrite; the exchange and whiteout flags are not implemented and have to
# say so rather than destroying the target.
check "files for the rename flags" bash -c "echo one > '$MNT/rf1' && echo two > '$MNT/rf2'"
check "mv -n refuses to overwrite" bash -c "mv -n '$MNT/rf1' '$MNT/rf2' 2>/dev/null; [ \"\$(cat '$MNT/rf2')\" = two ]"
check "and left the source alone"  test -e "$MNT/rf1"
check "renameat2 flag behaviour (python)" python3 - "$MNT" <<'PY'
import ctypes, ctypes.util, errno, os, sys
mnt = sys.argv[1]
libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
AT_FDCWD, RENAME_NOREPLACE, RENAME_EXCHANGE = -100, 1, 2

def rename2(a, b, flags):
    r = libc.renameat2(AT_FDCWD, a.encode(), AT_FDCWD, b.encode(), flags)
    return 0 if r == 0 else ctypes.get_errno()

a, b = f"{mnt}/rf1", f"{mnt}/rf2"
e = rename2(a, b, RENAME_NOREPLACE)
assert e == errno.EEXIST, f"NOREPLACE over an existing file: expected EEXIST, got {e}"
assert open(b).read() == "two\n" and os.path.exists(a), "NOREPLACE must change nothing"
e = rename2(a, b, RENAME_EXCHANGE)
assert e in (errno.EINVAL, errno.ENOSYS, errno.EOPNOTSUPP), f"EXCHANGE: expected a refusal, got {e}"
assert open(b).read() == "two\n" and open(a).read() == "one\n", "a refused EXCHANGE must change nothing"
assert rename2(a, f"{mnt}/rf3", RENAME_NOREPLACE) == 0, "NOREPLACE onto a free name must work"
assert open(f"{mnt}/rf3").read() == "one\n"
PY
check "clean up rename flag files" bash -c "rm -f '$MNT/rf1' '$MNT/rf2' '$MNT/rf3'"
# Dropping setuid/setgid and file capabilities on write, truncate and chown
# is coffer's own job since it negotiates FUSE_HANDLE_KILLPRIV_V2 (fs.rs
# init()). The kernel only asks for it when the caller lacks CAP_FSETID, so
# as root the correct answer is to keep the bits - the checks would be
# testing the opposite thing and are skipped there.
if [ "$(id -u)" != 0 ]; then
    check "setuid survives until something writes" bash -c "echo s > '$MNT/suid' && chmod 4755 '$MNT/suid' && [ \"\$(stat -c %a '$MNT/suid')\" = 4755 ]"
    check "a write drops setuid"        bash -c "echo more >> '$MNT/suid' && wait_mode '$MNT/suid' 755"
    check "a truncate drops setuid"     bash -c "chmod 4755 '$MNT/suid' && truncate -s 2 '$MNT/suid' && wait_mode '$MNT/suid' 755"
    check "setgid+group-x is dropped too" bash -c "chmod 2755 '$MNT/suid' && echo x >> '$MNT/suid' && wait_mode '$MNT/suid' 755"
    # S_ISGID without group-execute is not a privilege but a mandatory-
    # locking marker, and Linux leaves it alone; so must coffer.
    check "setgid without group-x survives a write" bash -c "chmod 2644 '$MNT/suid' && echo x >> '$MNT/suid' && sleep 1.5 && [ \"\$(stat -c %a '$MNT/suid')\" = 2644 ]"
    check "an explicit chmod u+s is not undone by its own request" bash -c "chmod 4755 '$MNT/suid' && sleep 1.5 && [ \"\$(stat -c %a '$MNT/suid')\" = 4755 ]"
    # kill_priv() also removes the security.capability attribute, which
    # cannot be checked from here: setting it needs CAP_SETFCAP, so an
    # unprivileged process is refused by the kernel before the request ever
    # reaches the filesystem (on ext4 just the same), and a privileged one
    # would not have the kernel ask for the kill in the first place. The
    # path matters for a file capability brought in by a root-run
    # `rsync -X`, which a later unprivileged write must drop.
    check "chown drops setuid as well" bash -c "chmod 4755 '$MNT/suid' && chown \"\$(id -u)\" '$MNT/suid' && wait_mode '$MNT/suid' 755"
    check "clean up" rm "$MNT/suid"
else
    echo "  skip setuid/capability checks (running as root, the kernel would not ask for them)"
fi
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
# What --locking auto picks on NFS, forced here so it is tested on any disk:
# the mount holds its locks for its whole lifetime, so nothing else can open
# the container and backup goes through the daemon's control socket.
section "exclusive locking"
sockets() { ls /run/user/"$(id -u)"/coffer/*.sock /tmp/coffer-"$(id -u)"/*.sock 2>/dev/null | sort; }
SOCKETS_BEFORE=$(sockets)
check "mount --locking exclusive" mnt "$V" "$MNT" --locking exclusive
# fcntl() locks don't survive a fork, so this is what proves the daemon -
# not the process that forked it - holds them.
check "the daemon holds a POSIX lock on the container" bash -c "grep -q \"POSIX .* \$(pgrep -f 'coffer mount $V ' | head -1) [0-9a-f:]*:\$(stat -c %i '$V') \" /proc/locks"
check "backup through the mount" bash -c "'$COFFER' backup '$V' '$WORK/backup-x.coffer' --password-file '$PW' | grep -q 'by the running mount'"
# script(1) gives the backup a terminal, which is what turns the bar on.
check "progress bar on a terminal" bash -c "script -qec \"'$COFFER' backup '$V' '$WORK/backup-p.coffer' --password-file '$PW'\" /dev/null | tr '\\r' '\\n' | grep -q '^\\[=*\\] 100%'"
eq "no progress bar without one" "" "$("$COFFER" backup "$V" "$WORK/backup-q.coffer" --password-file "$PW" 2>&1 >/dev/null)"
expect_fail "backup with the wrong password" "$COFFER" backup "$V" "$WORK/backup-bad.coffer" --password-file "$PW2"
check "wrong password left no file" test ! -e "$WORK/backup-bad.coffer"
check "info says it is mounted" bash -c "'$COFFER' info '$V' --password-file '$PW' 2>&1 | grep -q 'is mounted'"
check "writes go on while mounted" bash -c "echo still > '$MNT/x-after-backup'"
check "umount"               umnt "$V" "$MNT"
eq "socket removed on unmount" "$SOCKETS_BEFORE" "$(sockets)"
check "exclusive backup passes check" "$COFFER" check "$WORK/backup-x.coffer" --password-file "$PW"
check "mount the copy read-only" mnt "$WORK/backup-x.coffer" "$WORK/mnt2" --read-only
check "copy has the tree"    diff -r "$WORK/src" "$WORK/mnt2/tree"
check "umount the copy"      umnt "$WORK/backup-x.coffer" "$WORK/mnt2"
check "container fine afterwards" "$COFFER" check "$V" --password-file "$PW"
check "mount"                mnt "$V" "$MNT"
check "rm the extra file"    rm "$MNT/x-after-backup"
check "umount"               umnt "$V" "$MNT"

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
section "permission enforcement"
# Without --enforce-permissions the stored mode is reported but never
# checked; with it the kernel applies the ordinary rules. Root bypasses
# those checks, so as root the checks would prove nothing.
check "mount (default: not enforced)" mnt "$V" "$MNT"
check "a file with mode 000"  bash -c "echo secret > '$MNT/noperm' && chmod 000 '$MNT/noperm'"
eq "readable anyway" "secret" "$(cat "$MNT/noperm" 2>/dev/null)"
check "umount"                umnt "$V" "$MNT"
check "mount --enforce-permissions" mnt "$V" "$MNT" --enforce-permissions
if [ "$(id -u)" != 0 ]; then
    expect_fail "mode 000 is refused now"      cat "$MNT/noperm"
    check "chmod 644 makes it readable again"  bash -c "chmod 644 '$MNT/noperm' && [ \"\$(cat '$MNT/noperm')\" = secret ]"
    check "a directory with mode 000"          bash -c "mkdir -p '$MNT/nodir' && echo x > '$MNT/nodir/f' && chmod 000 '$MNT/nodir'"
    expect_fail "entering it is refused"       ls "$MNT/nodir"
    check "chmod 755 opens it again"           bash -c "chmod 755 '$MNT/nodir' && ls '$MNT/nodir' >/dev/null"
else
    echo "  skip the refusal checks (running as root, which bypasses permission checks)"
fi
check "umount"                umnt "$V" "$MNT"
check "create --save stores it"  bash -c "'$COFFER' create '$WORK/ep.coffer' --save ep --mountpoint '$WORK/mnt2' --enforce-permissions --password-file '$PW' >/dev/null && grep -q '^enforce_permissions = true' '$COFFER_CONFIG'"
check "add --enforce-permissions too" bash -c "'$COFFER' add ep2 '$V' '$WORK/mnt2' --enforce-permissions --password-file '$PW' >/dev/null && grep -c '^enforce_permissions = true' '$COFFER_CONFIG' | grep -qx 2"
check "mounting the alias enforces" bash -c "'$COFFER' mount ep --password-file '$PW' >/dev/null && wait_mounted '$WORK/mnt2'"
if [ "$(id -u)" != 0 ]; then
    expect_fail "and a mode 000 file stays refused there" bash -c "echo s > '$WORK/mnt2/x' && chmod 000 '$WORK/mnt2/x' && cat '$WORK/mnt2/x'"
fi
check "umount the alias"      bash -c "'$COFFER' umount ep >/dev/null && wait_unmounted '$WORK/mnt2' && wait_lock_free '$WORK/ep.coffer'"
check "remove the test aliases" bash -c "'$COFFER' remove ep >/dev/null && '$COFFER' remove ep2 >/dev/null"

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
section "hard links"
check "mount"                 mnt "$V" "$MNT"
check "a file to link to"     bash -c "echo linked > '$MNT/hl1'"
eq "one name to start with"   "1" "$(stat -c %h "$MNT/hl1")"
check "ln creates a second name" ln "$MNT/hl1" "$MNT/hl2"
eq "link count is 2"          "2" "$(stat -c %h "$MNT/hl1")"
eq "and the same from either name" "2" "$(stat -c %h "$MNT/hl2")"
eq "same inode"               "$(stat -c %i "$MNT/hl1")" "$(stat -c %i "$MNT/hl2")"
eq "same content"             "linked" "$(cat "$MNT/hl2")"
check "a write through one name" bash -c "echo more >> '$MNT/hl1'"
eq "is visible through the other" "2" "$(wc -l < "$MNT/hl2")"
check "chmod through one name"   chmod 640 "$MNT/hl2"
eq "shows on the other"       "640" "$(stat -c %a "$MNT/hl1")"
check "a third name in a subdirectory" bash -c "mkdir -p '$MNT/hldir' && ln '$MNT/hl1' '$MNT/hldir/hl3'"
eq "link count is 3"          "3" "$(stat -c %h "$MNT/hl1")"
check "removing one name"     rm "$MNT/hl2"
eq "leaves the others"        "2" "$(stat -c %h "$MNT/hl1")"
eq "content still there"      "2" "$(wc -l < "$MNT/hldir/hl3")"
check "renaming a name"       mv "$MNT/hldir/hl3" "$MNT/hldir/hl4"
eq "does not change the count" "2" "$(stat -c %h "$MNT/hl1")"
check "removing the first name" rm "$MNT/hl1"
eq "the last name has count 1" "1" "$(stat -c %h "$MNT/hldir/hl4")"
eq "and still the content"    "2" "$(wc -l < "$MNT/hldir/hl4")"
expect_fail "hard-linking a directory is refused" ln "$MNT/hldir" "$MNT/hldirlink"
expect_fail "linking onto an existing name is refused" bash -c "echo x > '$MNT/taken' && ln '$MNT/hldir/hl4' '$MNT/taken'"
check "readdir sees both names of one inode" bash -c "ln '$MNT/hldir/hl4' '$MNT/hldir/hl5' && [ \"\$(ls '$MNT/hldir' | wc -l)\" = 2 ]"
check "cp -l inside the mount"   bash -c "cp -l '$MNT/hldir/hl4' '$MNT/hldir/hl6'"
eq "three names now"          "3" "$(stat -c %h "$MNT/hldir/hl4")"
check "find -samefile finds them all" bash -c "[ \"\$(find '$MNT/hldir' -samefile '$MNT/hldir/hl4' | wc -l)\" = 3 ]"
check "rsync -H preserves the link" bash -c "rm -rf '$WORK/hlout' && rsync -aH '$MNT/hldir/' '$WORK/hlout/' && [ \"\$(stat -c %h '$WORK/hlout/hl4')\" = 3 ]"
check "umount"                umnt "$V" "$MNT"
check "remount"               mnt "$V" "$MNT"
eq "link count survives a remount" "3" "$(stat -c %h "$MNT/hldir/hl4")"
eq "same inode after remount"  "$(stat -c %i "$MNT/hldir/hl4")" "$(stat -c %i "$MNT/hldir/hl6")"
# One readdir call fills a fixed buffer, so a directory this size takes
# several - and the entries come out ordered by name, which is what makes
# the offset handed to the next call mean the same thing. 200 of the
# entries below share a single inode, so an ordering that was not unique
# within the directory would drop or repeat entries here.
check "a directory too big for one readdir call" fill_dir "$MNT/bigdir" 600
eq "every entry is listed once"  "600" "$(ls -U "$MNT/bigdir" | sort -u | wc -l)"
# -U is readdir order, unsorted by ls: this is the filesystem's own answer.
eq "readdir returns them in name order" "f0000" "$(ls -U "$MNT/bigdir" | head -1)"
check "200 names on one inode in it" link_many "$MNT/bigdir/shared" 199
eq "all of them are listed"      "800" "$(ls -U "$MNT/bigdir" | sort -u | wc -l)"
eq "and the link count is right" "200" "$(stat -c %h "$MNT/bigdir/shared")"
check "scandir sees each entry exactly once" bash -c "python3 -c 'import os,sys
with os.scandir(sys.argv[1]) as it: n=[e.name for e in it]
sys.exit(0 if len(n)==len(set(n))==800 else 1)' '$MNT/bigdir'"
check "removing them all"        rm -r "$MNT/bigdir"
check "deleting the whole directory" rm -r "$MNT/hldir" "$MNT/taken"
check "umount"                umnt "$V" "$MNT"
check "check after hard links" "$COFFER" check "$V" --password-file "$PW"
check "and it really checked the filesystem" bash -c "'$COFFER' check '$V' --password-file '$PW' 2>&1 | grep -q 'no dangling names, no unreachable files'"

# ---------------------------------------------------------------------------
# Cross-version behaviour, when a previous release is available
# (COFFER_PREV=/path/to/older/coffer). Since container format 2 the
# contract changed: a container the older version made is upgraded on its
# first writable open and is then no longer readable by that version - so
# what is tested here is that the upgrade keeps everything, and that the
# older version refuses the result cleanly instead of misreading it.
if [ -n "${COFFER_PREV:-}" ] && [ -x "$COFFER_PREV" ]; then
    section "upgrading a container from $("$COFFER_PREV" --version)"
    OLDV="$WORK/old.coffer"; OLDM="$WORK/mnt-old"; mkdir -p "$OLDM"
    check "previous version creates a container" "$COFFER_PREV" create "$OLDV" --password-file "$PW"
    # Whether the previous release can store extended attributes decides
    # what the upgrade has to carry over: the migration drops and recreates
    # the inodes table, which takes the xattrs_gc trigger with it, so an
    # attribute written by the *old* binary is the sharpest test of that.
    # Still optional, so an older COFFER_PREV than 0.1.3 keeps working.
    PREV_XATTR=no
    check "previous version fills it" bash -c "'$COFFER_PREV' mount '$OLDV' '$OLDM' --password-file '$PW' >/dev/null && wait_mounted '$OLDM' && mkdir -p '$OLDM/sub/deeper' && echo old > '$OLDM/from-old' && echo deep > '$OLDM/sub/deeper/f' && ln -s from-old '$OLDM/alink' && { setfattr -n user.old -v oldvalue '$OLDM/from-old' 2>/dev/null && echo yes > '$WORK/prev-xattr' || true; } && '$COFFER_PREV' umount '$OLDM' >/dev/null && wait_unmounted '$OLDM' && wait_lock_free '$OLDV'"
    [ -s "$WORK/prev-xattr" ] && PREV_XATTR=yes
    echo "  (previous version stores xattrs: $PREV_XATTR)"
    check "this version mounts it (and upgrades)" mnt "$OLDV" "$OLDM"
    eq "the old file is there"    "old" "$(cat "$OLDM/from-old")"
    eq "and the nested one"       "deep" "$(cat "$OLDM/sub/deeper/f")"
    eq "the symlink too"          "from-old" "$(readlink "$OLDM/alink")"
    if [ "$PREV_XATTR" = yes ]; then
        eq "the old version's xattr survived the upgrade" "oldvalue" \
            "$(getfattr -n user.old --only-values "$OLDM/from-old" 2>/dev/null)"
    fi
    check "and it is writable"    bash -c "echo new > '$OLDM/from-new' && setfattr -n user.k -v v '$OLDM/from-new'"
    check "hard links work on the upgraded container" bash -c "ln '$OLDM/from-new' '$OLDM/from-new-2' && [ \"\$(stat -c %h '$OLDM/from-new')\" = 2 ]"
    check "umount"                umnt "$OLDV" "$OLDM"
    expect_fail "the previous version now refuses it" "$COFFER_PREV" check "$OLDV" --password-file "$PW"
    check "and says why"          bash -c "'$COFFER_PREV' check '$OLDV' --password-file '$PW' 2>&1 | grep -q 'container format version 2'"
    expect_fail "it cannot mount it either" bash -c "'$COFFER_PREV' mount '$OLDV' '$OLDM' --password-file '$PW'"
    check "this version mounts it again" mnt "$OLDV" "$OLDM"
    eq "everything is still there" "old" "$(cat "$OLDM/from-old")"
    eq "our xattr survived"       "v" "$(getfattr -n user.k --only-values "$OLDM/from-new" 2>/dev/null)"
    eq "and the hard link"        "2" "$(stat -c %h "$OLDM/from-new")"
    check "the recreated xattrs trigger still collects" bash -c "setfattr -n user.gone -v g '$OLDM/sub/deeper/f' && rm '$OLDM/sub/deeper/f' && echo again > '$OLDM/sub/deeper/f' && [ -z \"\$(getfattr -d '$OLDM/sub/deeper/f' 2>/dev/null)\" ]"
    check "umount"                umnt "$OLDV" "$OLDM"
    check "upgraded container passes check" "$COFFER" check "$OLDV" --password-file "$PW"
    # A read-only mount cannot upgrade, so it has to say so rather than
    # fail with something unhelpful deeper down.
    check "a fresh version-1 container"   bash -c "'$COFFER_PREV' create '$WORK/old2.coffer' --password-file '$PW' >/dev/null"
    expect_fail "read-only mount of it is refused" bash -c "'$COFFER' mount '$WORK/old2.coffer' '$OLDM' -r --password-file '$PW'"
    check "with a message naming the way out" bash -c "'$COFFER' mount '$WORK/old2.coffer' '$OLDM' -r --password-file '$PW' 2>&1 | grep -q 'container format 1'"
    check "info still works on it"        "$COFFER" info "$WORK/old2.coffer" --password-file "$PW"
    check "check still works on it"       "$COFFER" check "$WORK/old2.coffer" --password-file "$PW"
    # The documented way back: a backup taken before the first writable
    # mount is still format 1, so an older installation can open it.
    check "backup of it still works"      "$COFFER" backup "$WORK/old2.coffer" "$WORK/old2-bk.coffer" --password-file "$PW"
    check "and the previous version opens that backup" "$COFFER_PREV" check "$WORK/old2-bk.coffer" --password-file "$PW"
    check "the original is untouched until then" "$COFFER_PREV" check "$WORK/old2.coffer" --password-file "$PW"
    # Documented: it is not only mount. compact opens writable, so it
    # upgrades too - which is what the refused read-only mount suggests.
    check "a fresh version-1 container for compact" bash -c "'$COFFER_PREV' create '$WORK/old3.coffer' --password-file '$PW' >/dev/null"
    check "compact upgrades it as well"   bash -c "'$COFFER' compact '$WORK/old3.coffer' --password-file '$PW' >/dev/null 2>&1"
    expect_fail "and the previous version then refuses it" "$COFFER_PREV" check "$WORK/old3.coffer" --password-file "$PW"
    # Into a file, not a pipe: a successful mount daemonizes, and grep -q
    # closing the pipe on its first match pulls the stdio out from under
    # the daemon. The upgrade line is printed before the fork either way.
    check "a writable mount says it is upgrading" bash -c "'$COFFER' mount '$WORK/old2.coffer' '$OLDM' --password-file '$PW' > '$WORK/upgrade.log' 2>&1 && wait_mounted '$OLDM' && grep -q 'from container format 1 to 2' '$WORK/upgrade.log'"
    check "umount"                umnt "$WORK/old2.coffer" "$OLDM"
    check "read-only works afterwards" bash -c "'$COFFER' mount '$WORK/old2.coffer' '$OLDM' -r --password-file '$PW' >/dev/null && wait_mounted '$OLDM'"
    check "umount"                umnt "$WORK/old2.coffer" "$OLDM"
else
    echo
    echo "== compatibility: NOT RUN (no COFFER_PREV)"
    if [ -n "${COFFER_PREV_OPTIONAL:-}" ]; then
        echo "  allowed by COFFER_PREV_OPTIONAL"
    else
        # A skipped cross-version section must not look like a green run:
        # it is the only proof that an older container still upgrades
        # cleanly. Set COFFER_PREV_OPTIONAL=1 where there is no previous
        # release to test against (a fork, a first release).
        bad "cross-version section skipped (set COFFER_PREV, or COFFER_PREV_OPTIONAL=1 to allow)"
    fi
fi

# ---------------------------------------------------------------------------
echo
echo "passed: $pass  failed: $fail"
exit "$fail"
