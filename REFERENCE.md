# cofferFS reference

The long version. [README.md](README.md) covers installation and everyday
use; this file holds the design rationale, the operational details and the
packaging notes behind it.

## How it works

`vault.coffer` is a single [SQLCipher](https://www.zetetic.net/sqlcipher/)
(encrypted SQLite) database. Directories and files are rows in that
database; file content is stored in 128 KiB chunks. Mounting is done via
[FUSE](https://github.com/libfuse/libfuse) through the `fuser` crate, which
is why no root privileges are required: FUSE mounts are owned by, and only
accessible to, the user who created them.

### Why this design, and what "auto-grow" means here

The container has **no fixed size**. SQLite simply extends the backing file
as you write more data into it — there is no `--auto-grow` flag to set and
no resize step to run, ever. That also means there's no risky "grow the
filesystem live" operation, which is usually where these containers get
corrupted. Optionally cap it with `--max-size` at creation time if you want
a hard ceiling (e.g. `coffer create vault.coffer --max-size 10G`); writes
past that are rejected with ENOSPC instead of silently eating your whole
disk.

### Why this doesn't destroy your data on a crash

This was the main ask, so it's worth spelling out:

1. **WAL journaling.** The container runs in SQLite's
   [Write-Ahead Log](https://sqlite.org/wal.html) mode with
   `synchronous=NORMAL`. This is one of the most heavily crash-tested
   on-disk formats in existence — SQLite ships with a dedicated crash-
   simulation test suite for exactly this property. A `kill -9` of the
   mount process mid-write loses at most the last unflushed write; it does
   not corrupt the container. This was verified directly while building
   this tool: 1000+ files were written, the mount process was hard-killed
   mid-write, and `coffer check` afterwards reported no corruption with
   every previously-written file byte-for-byte intact.
2. **Per-page HMAC integrity.** SQLCipher attaches an HMAC to every 4 KiB
   page. `coffer check <file>` runs `PRAGMA cipher_integrity_check` (catches
   tampering/bit-rot at the encryption layer) and `PRAGMA integrity_check`
   (catches structural corruption) without modifying anything. Note: SQLCipher
   itself has a confirmed upstream bug where `cipher_integrity_check`
   misreports every page past the 4GB mark as HMAC-failed on containers
   larger than that (reproduced independently of this project, across two
   separate SQLCipher builds - not something we can fix here). `coffer check`
   detects this specific false-positive pattern and treats `integrity_check`
   (which actually decrypts and verifies every page) as authoritative, so it
   won't cry wolf on a large, healthy container - while still correctly
   failing on real corruption anywhere in the file.
3. **Live, consistent backups.** `coffer backup <file> <dest>` uses
   SQLite's online backup API to make a byte-consistent copy — safe to run
   even while the container is mounted and being written to. Cheap
   insurance; consider cronning it.
4. **Wrong password fails loudly.** SQLCipher refuses to open the database
   at all if the passphrase is wrong (HMAC check on page 1 fails
   immediately) rather than silently returning garbage.

### Encryption

SQLCipher defaults: AES-256-CBC per page + HMAC-SHA512, key derived from
your password with PBKDF2. This is solid, standard, "not the point of the
project" encryption — nobody had to hand-roll a cipher for this to work
well. Change the password any time with `coffer passwd <file>`.

### Handling many files

File and directory metadata live in an indexed SQLite table
(`UNIQUE(parent, name)` + an index on `parent`), so lookups and `readdir`
stay fast regardless of how many files are inside. Reading/listing was never
the bottleneck at any scale tested (`find -type f | wc -l` over 95,000 files
takes well under a second - `readdir()` reports a real file type per entry,
so tools like `find` don't need an extra syscall per file just to answer
`-type f`).

See **Performance at scale** below for the full write-throughput benchmark.


### Extended attributes

Extended attributes (`user.*`, `security.*`, whatever a tool sets) are
stored per inode in their own table, `xattrs`, with the same limits Linux
itself applies (255-byte names, 64 KiB values, `E2BIG`/`ERANGE` beyond
that) and the same `XATTR_CREATE`/`XATTR_REPLACE` semantics as
`setxattr(2)`. They follow the inode through renames, are deleted with
it, and travel with `cp --preserve=xattr`, `rsync -X` and `coffer backup`.

The table is an additive extra, not a schema version bump: a writable open
creates it when missing (`CREATE TABLE IF NOT EXISTS`), and a version of
`coffer` from before 0.1.3 never looks at it - it only reads `meta`,
`inodes` and `data` - so containers keep opening in both directions. A
trigger created alongside the table (`xattrs_gc`, after delete on
`inodes`) drops an inode's attributes whenever its row goes, and since the
trigger lives in the database it also fires for an older `coffer` that
unlinks the file, so no orphan rows are left behind either way. A
read-only mount of a container that predates the table simply reports no
attributes. Names must be valid UTF-8, like file names in a container.
The integration suite exercises exactly this cross-version round trip
against the 0.1.2 release, in both directions.

One consequence of answering `getxattr` at all: the kernel would then ask
for `security.capability` before every buffered write, to know whether
file capabilities must be dropped - an extra round trip per `write(2)`,
serialised on the single FUSE thread. `coffer` therefore negotiates
`FUSE_HANDLE_KILLPRIV_V2` (Linux 5.11+) and does that job itself: a write
by a process without `CAP_FSETID` clears the setuid/setgid bits and the
`security.capability` attribute, as on any other filesystem, and the
kernel stops asking. Older kernels refuse the capability and keep the
round trip.

## Registered vaults (`~/.coffer/config`)

Typing the container path and the mountpoint on every mount gets old fast,
so `coffer` keeps a small per-user registry of file/mountpoint pairs, each
under an alias. Register a vault in whichever way fits the moment:

```bash
coffer add work ~/.coffer/work.coffer ~/vault        # register without mounting
coffer mount ~/.coffer/work.coffer ~/vault --save work   # register while mounting
coffer create ~/.coffer/work.coffer --save work --mountpoint ~/vault   # register while creating
```

From then on the alias stands in for the file (and, for `mount`/`umount`,
the mountpoint too):

```bash
coffer mount work
coffer umount work
coffer info work          # same for check, backup, passwd, compact
```

And with no argument at all, `coffer mount` and `coffer umount` do the
obvious thing: if exactly one vault is registered, `mount` mounts it; if
exactly one registered vault is currently mounted, `umount` unmounts it.
With several to choose from you get a numbered menu - alias, mountpoint,
file, size on disk, last modified, and whether it's mounted right now -
and type the number (or the alias). If there's no terminal to ask on (a
script, a cron job), that's an error telling you to name the alias instead,
never a guess.

`coffer list` prints the same table without asking anything, and `coffer
remove work` forgets an alias without touching the container file. Options
given to `coffer add` or alongside `--save` (`--idle-timeout`,
`--compact-on-idle`, `--password-file`) are stored with the entry and become
that alias's defaults, so `coffer mount work` can mean "mount it and
auto-unmount after 30 idle minutes" without repeating the flag - anything
passed on the command line still wins over the stored value.

The file itself is deliberately plain - one section per vault, the section
name being the alias - and safe to edit by hand: comments on their own line
survive `coffer add`/`remove`, and a leading `~` in a path means `$HOME`.

```ini
[work]
file = /home/me/.coffer/work.coffer
mountpoint = /home/me/vault
idle_timeout = 30m

[photos]
file = /data/photos.coffer
mountpoint = /media/photos
```

Two rules keep this predictable. An alias is a bare word (letters, digits,
`-`, `_`, `.`, no leading `.` or `-`), so an argument containing a `/` or
starting with `.` or `~` is always a path, never looked up - `./work` means
the file even if an alias `work` exists. And a bare word that matches a
registered alias is the alias; if it matches nothing, it's tried as a path.
`coffer add` refuses a container file that doesn't exist (a typo should fail
right there, not at the next mount), stores both paths absolute, and
rewrites the file atomically with mode `0600` (a freshly created
`~/.coffer` gets `0700`), since entries can name password files. A
different location can be pointed at with `$COFFER_CONFIG`. A broken
registry never gets in the way of the classic path-based forms: `coffer
mount <file> <mountpoint>` and `coffer umount <mountpoint>` don't read it.

## NFS home directories

If `$HOME` is NFS-mounted with `root_squash` (common on shared workstations
and clusters), unmounting a container whose mountpoint lives under your home
directory can fail even though you're the one who mounted it: fusermount3's
setuid-root helper briefly runs as effective root to call `umount2()`, NFS
maps that squashed root down to `nobody`, and FUSE's owner check then
rejects it. (This is different from a plain "device or resource busy" -
`coffer umount` already retries those automatically with a lazy unmount, no
sudo needed; see **Design notes** below.) `coffer umount` prints a
`sudo umount <path>` fallback when the root_squash case above happens, but
it's simplest to just avoid the situation: create and mount the
container on a local, non-NFS filesystem instead, e.g. under `/tmp`, and
symlink it back into your home directory for convenience:

```bash
mkdir -p /tmp/vault/dev
coffer create /tmp/vault/dev/vault.coffer
coffer mount  /tmp/vault/dev/vault.coffer /tmp/vault/dev/mnt
ln -sT /tmp/vault/dev ~/dev
```

`~/dev` now transparently resolves to the local working copy, but the
container and its mountpoint never touch NFS, so unmounting works normally
without `sudo`. Keep in mind `/tmp` is typically cleared on reboot, so if the
container itself (not just the mountpoint) needs to survive a reboot, put it
somewhere local but persistent instead.

## Auto-unmount on idle

`coffer mount --idle-timeout 30m` (accepts `s`/`m`/`h`/`d` suffixes, e.g.
`45s`, `2h`) unmounts the container itself after it's seen no filesystem
activity for that long - no separate daemon, cron job, or systemd timer
needed. This is tracked inside the FUSE process: every handled call
(open, read, write, readdir, ...) refreshes a last-activity timestamp, and
a background thread in the same process polls it and shells out to
`fusermount3 -u` once the idle threshold is crossed. Since it's the mount's
own owner unmounting it, this doesn't hit the NFS/`root_squash` wrinkle
described above.

Off by default - pass `--idle-timeout` explicitly to opt in. Note that
*any* filesystem call counts as activity, including ones triggered by
something other than you directly (a backup tool or file indexer
periodically scanning the mount will keep resetting the timer).

## Reclaiming disk space

Deleting files inside a container frees their rows in the underlying
SQLite database, but the `.coffer` file itself doesn't shrink on its own -
SQLite just adds that freed space to an internal free-list and reuses it
for future writes. That's normal SQLite behavior, not a bug, but it means
the file on disk can stay much bigger than what's actually inside it after
deleting something large (an old backup, a big video, a subtree you
cleaned up).

`coffer info` shows both numbers so you can tell if it's worth doing:

```
On-disk size:      14000000000 bytes
Logical data used:  2000000000 bytes
```

A big gap there is what `coffer compact <file>` (a `VACUUM`) reclaims, by
rewriting the file without the freed space. You'd generally only reach for
this after a large deletion, not as routine maintenance - day-to-day
writes reuse that freed space automatically, so compacting a container
that's just been growing steadily has nothing to gain. It needs up to
roughly twice the container's current size in free disk space while it
runs: `VACUUM` builds the compacted copy in a temporary database, which
`coffer` places next to the container (not in `/tmp`, which is often a
RAM-backed tmpfs, and not in memory - SQLite's bundled default would keep
it there, so a 10 GB container would have needed 10 GB of RAM). That
temporary copy is encrypted with the container's own key; SQLCipher keys
every database attached without an explicit key with the main database's
key, and that is exactly how `VACUUM` attaches it. `compact` refuses to
run against a mounted container - same reasoning as `passwd`, see
**Design notes** below.

If you'd rather not think about it at all, `coffer mount --compact-on-idle
1h` does this automatically while mounted: once the mount has been idle
that long, it checks for a meaningful gap (at least 64MB *and* at least
10% of the file) and only then runs `VACUUM` - most idle periods have
nothing worth reclaiming, so it stays a no-op most of the time rather than
rewriting the file on every idle tick. It keeps running afterward (unlike
`--idle-timeout`, which unmounts and stops), so a later deletion can be
reclaimed on a future idle period too. One real caveat, and it's bigger
than it sounds: `fuser` dispatches FUSE requests from a single thread by
default, so a `VACUUM` mid-run doesn't just block the next *write* - it
blocks *everything* (`ls`, `stat`, opening a file, all of it) for as long
as it takes, on a large container potentially minutes. It re-checks
right before starting that the mount is still idle (in case activity
resumed in the moment between deciding to compact and actually acquiring
the lock), which narrows but can't fully close that window. If that
tradeoff doesn't sit right for a container you use interactively, prefer
running `coffer compact` yourself while you're not using the mount instead
of `--compact-on-idle`.

## Design notes

- Inode numbers are `AUTOINCREMENT` (never reused), avoiding a FUSE
  inode-reuse hazard.
- Every mutating FUSE call (write, mkdir, rename, ...) is wrapped in one
  explicit SQLite transaction and committed once, so a single filesystem
  operation is always all-or-nothing.
- `mount` daemonizes by default (returns control to the shell immediately);
  pass `--foreground` to keep it attached for debugging.
- `umount` (and the idle-timeout watcher) automatically retries a failed
  unmount with a lazy unmount (`fusermount3 -u -z`) before giving up - this
  is what resolves the common "device or resource busy" case (e.g. a crashed
  mount daemon the kernel hasn't finished tearing down yet), entirely within
  the mounting user's own permissions, no sudo involved. The plain attempt's
  own error output is suppressed so a routine escalation doesn't look like a
  failure; only a genuine final failure prints anything.
- A lazy unmount alone only *detaches* the mountpoint; the mount daemon
  keeps running until the last process referencing the mount lets go - and
  that can be never: a mountpoint under `/tmp` is visible to everyone, and
  another user's desktop session (gvfs, file indexers) will happily hold a
  watch on it for days. Meanwhile the daemon still holds the container's
  exclusive lock, which on an NFS home is visible on every host - so
  `coffer mount` elsewhere fails with "already in use" although `umount`
  reported success. So after a lazy detach of one of its own mounts, `umount`
  also aborts the FUSE connection (`/sys/fs/fuse/connections/<id>/abort`,
  owned by the mounting user): the daemon's request loop ends exactly as on a
  normal unmount, it exits cleanly and the lock is released immediately;
  whatever still had the mount open gets I/O errors. It prints one line
  saying so. The connection id is taken from `/proc/self/mountinfo` *before*
  detaching - deliberately not via `stat()`, which on a FUSE mountpoint is
  itself a FUSE request and hangs if the daemon is wedged.
- Every `coffer` process marks itself non-dumpable (`PR_SET_DUMPABLE`):
  no core dumps, and no `ptrace` from other processes of the same user, so
  another program running under your account cannot read the password or
  the derived key out of a running mount. Root still can - a mounted
  container is plaintext to root by definition. Every buffer that holds a
  password is zeroed when it is dropped, so it does not linger in freed
  heap memory. Neither helps against a key that has been swapped out;
  encrypted swap is the system's job.
- Password prompts mask input on a real terminal; if stdin isn't a TTY
  (piping, scripting), it falls back to a visible plain-text read. Every
  command that takes a password also accepts `--password-file <path>` as an
  explicit alternative - mainly so the password never has to appear as a
  process argument or get piped through a `printf`/`echo` that's briefly
  visible in `ps`. A world/group-readable password file prints a warning
  (not a hard failure).
- `mount`, `passwd`, and `compact` take an exclusive `flock()` on the
  container file before doing anything else, so two of them can never run
  against the same container at once - the actual risk this project cares
  about (two live writers). `check`, `backup`, and `info` deliberately don't
  lock: WAL mode already gives them a safe, consistent view alongside an
  active writer, which `backup` in particular depends on. The lock is
  released by the kernel the instant every fd on it closes - including on a
  crash or `kill -9` - so unlike a PID file there's no stale-lock case to
  clean up; verified by killing a mount mid-session and confirming `passwd`
  could immediately acquire the lock right after. `mount` acquires this
  lock before it even opens the database (not just before daemonizing) -
  otherwise a concurrent `passwd` could rekey the container in the window
  between `mount` opening its connection and taking the lock, leaving
  `mount` serving with a stale key against a file now encrypted under a
  different one. This lock is only ever as reliable as `flock()` is on
  whatever filesystem the container lives on - solid locally, but
  historically inconsistent over NFS depending on version/lockd config, so
  don't rely on it as a hard guarantee for a container shared over NFS from
  multiple machines.
- Rust was chosen over C for the compiler's memory safety (removes an
  entire class of leak/use-after-free/buffer-overflow bugs in the block-
  storage code - not a risk worth taking for a tool whose whole point is
  *not* corrupting your data), while `fuser` and `rusqlite` give ergonomic,
  well-maintained bindings for FUSE and SQLCipher respectively.

## Verified while building this

Created a container, mounted it as a non-root user, wrote 1000 small files
plus a 20MB file (checksum-verified), unmounted and remounted to confirm
persistence, rejected a wrong password, ran `check`/`backup`/`passwd`
successfully, and hard-`kill -9`'d the mount process mid-write — the
container stayed structurally intact (`check` clean) with every
previously-written byte recoverable afterward.

## Performance at scale (95,000 files, ~12GB)

Benchmarked end to end at a realistic scale (95,000 files, mixed sizes
averaging ~126KB, ~12GB total) against a plain ext4 baseline for context:

| | population (write) | `find -type f \| wc -l` |
|---|---|---|
| ext4 (baseline) | 23.6s (4027 files/s, 509MB/s) | 0.05s |
| Rust (before tuning) | 463.5s (205 files/s, 26MB/s) | 0.94s |
| **Rust (tuned)** | **365.3s (260 files/s, 33MB/s)** | 0.94s |

Reading/listing was never the bottleneck at any scale tested. `fuser`
(this project's FUSE binding) negotiates a large `max_write` by default, so
large writes never get chunked into many small FUSE calls in the first
place. One real issue turned up while chasing write throughput at this
scale, since fixed: every FUSE call was re-parsing its SQL. `src/fs.rs` now
uses `Connection::prepare_cached` throughout instead of `execute`/`prepare`,
and `PRAGMA cache_size` is raised from SQLite's ~2MB default to 128MB
(SQLCipher has to re-decrypt+HMAC-verify a page every time it's evicted
from cache and re-read, so a bigger cache means less redundant crypto work
as the container grows). Together these cut the full 95,000-file/12GB run
from 463s to 365s (~27% faster).

Benchmark runs must be isolated to get repeatable numbers - run one mount
at a time on an otherwise idle machine.

## Known upstream SQLCipher bug (not ours, but worth knowing about)

While verifying integrity on the 12GB benchmark container, `coffer check`
reported hundreds of thousands of "corrupt" pages, all starting at exactly
page 1,048,577 - which, at 4096 bytes/page, is precisely the 4GB mark
(2^32 bytes). That's too precise to be real corruption, so it was run down:

- Reproduced with a **30-line program using nothing but `rusqlite` +
  SQLCipher** - no FUSE, no coffer schema, just a table with a BLOB column,
  written to past 4GB and integrity-checked. Same failure, same exact page.
- Reproduced identically across **two independent SQLCipher builds**:
  Ubuntu's `libsqlcipher-dev` package, and a from-source build via
  `bundled-sqlcipher-vendored-openssl`. Both fail at the exact same page
  number.
- Meanwhile `PRAGMA integrity_check` - which actually walks and decrypts
  every page to verify the B-tree structure, rather than just recomputing
  HMACs - reported `ok` every time. Files written well past the 4GB mark
  (including the very last file in a 12GB container) read back with
  correct, repeatable checksums.

Conclusion: `PRAGMA cipher_integrity_check` has a real bug in its own page-
iteration logic for databases past 4GB, independent of this project. The
data itself is fine. `coffer check` detects this specific pattern - a page
number past the 4GB boundary flagged by `cipher_integrity_check` while
`integrity_check` passes cleanly - and reports the container as healthy
with an explanatory note, while still failing loudly on genuine corruption
anywhere in the file (verified with a deliberate single-byte flip in a
small container: still caught, still exits non-zero).

Root cause and status: a 32-bit overflow in `sqlcipher_codec_ctx_integrity_
check()`'s page-offset calculation (`src/crypto.c`), which wraps exactly at
page 1,048,577 (4096-byte pages) - reported upstream as
[sqlcipher/sqlcipher#604](https://github.com/sqlcipher/sqlcipher/issues/604).
Confirmed by the maintainer as already fixed in SQLCipher 4.17.0. This
project vendors SQLCipher through `rusqlite`'s
`bundled-sqlcipher-vendored-openssl` feature (`libsqlite3-sys`), which as
of writing still bundles 4.14.0 (predates the fix), so `coffer check`'s
workaround above remains necessary until that crate updates its vendored
copy.

## Known limitations (honest scope)

- **Single mounter at a time.** The FUSE loop is intentionally
  single-threaded to keep the SQLite access pattern simple and correct.
  Mounting (or running `passwd`/`compact` against) an already-in-use
  container is rejected outright with a clear error - see the locking
  bullet in **Design notes** - rather than silently racing two writers.
- **No `default_permissions` enforcement.** Whoever can supply the
  password gets full read/write access to everything inside; Unix
  permission bits are stored and reported but not enforced. This matches
  the personal-container use case (VeraCrypt-style), not a multi-user
  shared filesystem.
- **Large files are fine, but not optimized for huge ones.** Reads/writes
  go through per-128KiB-block SQL statements. Great for documents, photos,
  typical file collections; you would want a different design (e.g. a
  dedicated blob store) if you're routinely storing many multi-GB files.
- **Deleted space isn't reclaimed automatically.** See **Reclaiming disk
  space** above - `coffer compact` handles this, but it's an occasional,
  manual step, not something that happens on its own.
- **`cipher_integrity_check` false positives past 4GB.** See above -
  `coffer check` already works around this, but be aware if you ever run
  the raw `PRAGMA cipher_integrity_check` yourself against a large
  container.

## Packaging

Every package is built twice, for amd64/x86_64 and for arm64/aarch64. The
release workflow runs the arm64 builds on GitHub's hosted ARM runners
inside the arm64 variants of the same multi-arch container images, so
nothing is cross-compiled or emulated and each arm64 package is verified
exactly like its amd64 twin. The Debian arm64 packages are what Raspberry
Pi OS (64-bit) installs. 32-bit ARM (armhf, for Pi Zero/1/2) is not built:
it would need QEMU emulation in CI and the audience is small. The local
scripts below build whatever architecture the host has.

`packaging/build-deb.sh` builds **one `.deb` per target distro**, each
natively inside that distro's own container, via
[cargo-deb](https://github.com/kornelski/cargo-deb). Output goes to
`dist/coffer_<version>_<debian12|debian13|ubuntu2404|ubuntu2604>_amd64.deb`.
Every package also installs the `coffer(1)` man page
(`packaging/coffer.1`, gzipped by `make man` before `cargo deb` runs - see
the Makefile) to `/usr/share/man/man1/`.

```bash
packaging/build-deb.sh          # -> dist/coffer_*_<id>_amd64.deb (all 4)
packaging/build-rpm.sh          # -> dist/coffer-*-1.<dist>.x86_64.rpm (all 6, see below)
packaging/test-install.sh       # installs each into a matching fresh
                                 # container and runs a full create/mount/
                                 # write/read/umount/check cycle
```

**RPM targets** (`packaging/build-rpm.sh`, via
[cargo-generate-rpm](https://github.com/cat-in-136/cargo-generate-rpm))
follow the exact same per-distro pattern: Fedora 43 and 44, Enterprise
Linux 9 and 10 (built on AlmaLinux, binary-compatible with RHEL, Rocky and
Oracle), openSUSE Leap 16.0 and Tumbleweed. The distro id lands in the RPM
`Release` field (`coffer-0.1.0-1.fedora44.x86_64.rpm`), so the RPMs follow
the same spelled-out naming as the `.deb` files without a rename step -
RPM naming puts the `Release` field into the file name itself. `el9`/`el10`
is the one abbreviation kept, since those packages serve RHEL, AlmaLinux,
Rocky and Oracle alike. Tumbleweed being a rolling release, its RPM matches
Tumbleweed as of the build - the rolling `latest` prerelease (rebuilt on
every push to `main`) is the one to use there. The RPMs are not GPG-signed,
which `dnf` accepts for local files as is and `zypper` needs
`--allow-unsigned-rpm` for.

**Runtime dependencies.** Exactly two: the C library, and the `fuse3`
package for the `fusermount3` helper binary that `mount`/`umount` shell
out to. `fusermount3` is easy to miss since `ldd` only reports linked
libraries, not subprocesses - `test-install.sh` caught it by actually
exercising `mount`/`umount` rather than just checking that the install
succeeded. The `.deb` gets it from the explicit `depends` in `Cargo.toml`,
the RPMs from the explicit `requires` there; the C library dependency is
discovered automatically at build time by both.

There is deliberately no libfuse dependency. `fuser` is used with its
default, pure-Rust mount, which talks to `fusermount3` directly and links
no `libfuse3.so`. The first releases were built with fuser's `libfuse3`
feature instead, and that turned out to be the one runtime library that
differs between the target distros: Debian 12, Ubuntu 24.04, EL9/EL10 and
Fedora 43 ship libfuse 3.10-3.16 as SONAME 3 (`libfuse3-3`), while Debian
13, Ubuntu 26.04, Fedora 44 and Tumbleweed have 3.17+ as SONAME 4
(`libfuse3-4`, with no `libfuse3.so.3` compatibility symlink), and a binary
linked against one cannot load the other - `test-install.sh` caught a
Debian-12-built package whose `Depends: libfuse3-3` wasn't even resolvable
on Debian 13. It also printed a spurious `fuse: warning: library too old,
some operations may not work` on every SONAME-3 distro, because fuser
hands `fuse_session_new()` an ops struct sized for libfuse 3.17. Dropping
the feature removed both problems at once.

**Why still one package per distro.** With libfuse gone, a package built
on the oldest target would in principle install everywhere (glibc is
forward-compatible; `libc6 (>= 2.34)` from a Debian 12 build is satisfied
by all newer targets). Building natively per distro is kept anyway: it
costs nothing but CI minutes, every package is verified on exactly the
distro it is meant for, and each one declares the C library version that
distro actually ships instead of an artificially old floor.

SQLCipher and OpenSSL are statically bundled into every build (rusqlite's
`bundled-sqlcipher-vendored-openssl` feature) rather than linked against
the distro's `libsqlcipher-dev`, for two reasons: it removes a runtime
dependency that would otherwise need separate version tracking across ten
distros, and it sidesteps the upstream 4GB bug above living in whichever
SQLCipher build happens to be in a given distro's archive at the time.

All packages pass the full `test-install.sh` cycle (install, create, mount,
write, read, unmount, check) on every target and both architectures.

**Signatures.** The release job writes `SHA256SUMS` over every package and
signs that file with [minisign](https://jedisct1.github.io/minisign/)
(`SHA256SUMS.minisig`). A checksum file alone proves nothing - whoever can
replace a package on the download page can replace the checksums next to
it - so the signature is what to verify, against the public key in
`minisign.pub` at the repository root (and in the README). The signing
key is a plain minisign key stored only in the repository's Actions
secrets; the job verifies its own signature against the committed public
key before publishing, so a mismatch between the two fails the release
rather than shipping an unverifiable signature.

**Source packages for external repositories.** Two more definitions live
next to the binary packaging, for repositories that build from source on
their own infrastructure: `packaging/aur/PKGBUILD` for the Arch User
Repository and `packaging/rpm/coffer.spec` for COPR (Fedora, EL 9/10) or
any other rpmbuild-based service. Both build from the release tarball of a
pinned tag, hand build.rs the tag's commit through `COFFER_GIT_HASH` (there
is no `.git` in a tarball, and `coffer --version` should still name the
commit), and are built, installed and smoke-tested by the "Source
packages" workflow on Arch, Fedora 44, EL9 and EL10 whenever they change
and on every release tag. On a release, bump the version, the commit and
the checksum in both files in the same commit as `Cargo.toml`.

## Files in this repo

- `src/` — the FUSE filesystem + CLI implementation
- `Cargo.toml` / `Cargo.lock` — dependencies and the `cargo-deb` packaging
  metadata
- `Makefile` — `make build` / `make install` / `make man`
- `packaging/` — the `coffer(1)` man page and the per-distro `.deb` and
  `.rpm` build scripts (see **Packaging** above)
- `.github/workflows/release.yml` — builds a `.deb` and an `.rpm` per target
  distro, smoke-tests each one, and publishes them to GitHub Releases
