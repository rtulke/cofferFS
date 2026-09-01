# cofferFS

A growable, encrypted, single-file container you create and mount **as a
normal user — no root, no sudo**. Backed by SQLCipher (statically bundled,
no runtime dependency on a system SQLCipher package) and mounted via FUSE.
Once mounted it behaves like an ordinary directory: `cp`, `mv`, `rsync`,
file managers, all work normally.

```
coffer create vault.coffer
coffer mount  vault.coffer ~/vault
cp -r ~/Documents/secret-stuff ~/vault/
coffer umount ~/vault
```

`coffer --version` prints the package version *and* the exact git commit
it was built from, so two installs claiming the same version number can
still be told apart.

## Install

**Prebuilt `.deb`** — one package per target distro (Debian 12/13, Ubuntu
24.04/26.04), each built natively in that distro (see **Packaging** below
for why there's one per distro rather than a single universal package):

```bash
sudo apt-get install ./coffer_*_<debian12|debian13|ubuntu2404|ubuntu2604>_amd64.deb
```

Grab the matching `.deb` for your distro from the
[Releases page](https://github.com/rtulke/cofferFS/releases), or build them
all yourself (see **Packaging** below).

**From source:**

```bash
./setup.sh           # Debian 12/13, Ubuntu 24.04/26.04 - see below
make build           # after setup.sh once, this is all you need
sudo make install    # optional: installs to /usr/local/bin/coffer
                     # + the coffer(1) man page
```

`setup.sh` is idempotent (safe to re-run) and does three things:

1. Installs system packages via `apt`: `build-essential`, `pkg-config`,
   `perl`, `fuse3`, `libfuse3-dev`.
2. Installs a current Rust stable toolchain via [rustup](https://rustup.rs)
   under `$HOME/.cargo`. **This is required** — the `rustc`/`cargo` shipped
   in apt on these distros (1.63-1.75 depending on release) is too old for
   several of this project's dependencies, which need the 2024 edition.
   rustup's toolchain doesn't conflict with any distro package.
3. Runs `cargo build --release`.

SQLCipher and OpenSSL are compiled from source and statically linked in
(no `libsqlcipher-dev` needed) - see **Packaging** below for why.

If you're not on Ubuntu/Debian: install `libfuse3-dev`/`fuse3` (or the
equivalent) plus a Rust toolchain via [rustup.rs](https://rustup.rs), then
`cargo build --release`. macOS works via
[macFUSE](https://osxfuse.github.io/) (`brew install --cask macfuse`,
one-time security approval required).

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

## Usage

```bash
# Create a container (prompts for a password, unlimited size by default)
coffer create vault.coffer
coffer create vault.coffer --max-size 20G     # optional hard ceiling

# Mount / unmount (as yourself, no sudo)
coffer mount  vault.coffer ~/vault
coffer umount ~/vault

# Auto-unmount after being idle for a while (no fixed default - opt in)
coffer mount  vault.coffer ~/vault --idle-timeout 30m

# Auto-compact after being idle, but only if there's real space to reclaim
coffer mount  vault.coffer ~/vault --compact-on-idle 1h

# Maintenance
coffer info    vault.coffer      # file/dir counts, size on disk vs. logical data
coffer check   vault.coffer      # read-only integrity check (HMAC + structural)
coffer backup  vault.coffer vault.bak.coffer   # consistent copy, safe while mounted
coffer passwd  vault.coffer      # change the password
coffer compact vault.coffer      # VACUUM - reclaim space after deletions; refuses while mounted

# Non-interactive auth for scripts/cron/systemd units (any command that
# normally prompts accepts this instead)
coffer mount vault.coffer ~/vault --password-file ~/.coffer-pw

# Shell completions (bash/zsh/fish) - installed automatically by the .deb
# package and by `sudo make install`; only needed manually for a
# PREFIX=$HOME/.local source install, where the standard system completion
# directories aren't on the search path:
coffer completions bash > ~/.local/share/bash-completion/completions/coffer
```

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
runs (temporary, that's just how `VACUUM` works), and refuses to run
against a mounted container - same reasoning as `passwd`, see **Design
notes** below.

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

`packaging/build-deb.sh` builds **one `.deb` per target distro**, each
natively inside that distro's own container, via
[cargo-deb](https://github.com/kornelski/cargo-deb). Output goes to
`dist/coffer_<version>_<debian12|debian13|ubuntu2404|ubuntu2604>_amd64.deb`.
Every package also installs the `coffer(1)` man page
(`packaging/coffer.1`, gzipped by `make man` before `cargo deb` runs - see
the Makefile) to `/usr/share/man/man1/`.

```bash
packaging/build-deb.sh          # -> dist/coffer_*_<id>_amd64.deb (all 4)
packaging/test-install.sh       # installs each into a matching fresh
                                 # container and runs a full create/mount/
                                 # write/read/umount/check cycle
```

**Why per-distro and not one universal package:** the first attempt built a
single `.deb` inside `debian:12-slim` (the oldest target) on the theory that
glibc and libfuse3 are both forward-compatible - a binary built against an
older version runs fine on a newer system, so building against the oldest
target's libraries should make one package installable everywhere. That's
true for glibc (`libc6 (>= 2.34)` from the Debian 12 build is satisfied by
all four targets), but **not** for libfuse3: Debian 12 and Ubuntu 24.04 ship
it as SONAME 3 (package `libfuse3-3`), while Debian 13 and Ubuntu 26.04
bumped it to SONAME 4 (package `libfuse3-4`) - and critically, `libfuse3-4`
does *not* also ship a `libfuse3.so.3` symlink for backward compatibility.
A binary linked against SONAME 3 can't load SONAME 4 or vice versa. This
wasn't a theoretical concern - `test-install.sh` caught it immediately: the
Debian-12-built package's `Depends: libfuse3-3` wasn't even resolvable on a
fresh Debian 13 container, so `apt-get install` refused outright. Building
natively per distro sidesteps the whole question - each package just
depends on whatever that distro actually ships.

SQLCipher and OpenSSL are statically bundled into every build regardless
(rusqlite's `bundled-sqlcipher-vendored-openssl` feature) rather than linked
against the distro's `libsqlcipher-dev`, for two reasons: it removes a
runtime dependency that would otherwise need separate version tracking
across four distros, and it sidesteps the upstream 4GB bug above living in
whichever SQLCipher build happens to be in a given distro's archive at the
time. So libfuse3 ends up being the *only* runtime library dependency that
varies by target.

The only runtime dependencies are `libc6`, `libfuse3-3`/`libfuse3-4`
(whichever the target ships), and the `fuse3` package itself (for the
`fusermount3` helper binary that `mount`/`umount` actually shell out to -
it's a separate package from `libfuse3-N`, easy to miss since `ldd` only
reports the linked *library*, not the subprocess dependency; this was also
caught by `test-install.sh` actually exercising `mount`/`umount`, not just
checking that install succeeds).

All four packages pass the full `test-install.sh` cycle (install, create,
mount, write, read, unmount, check). On the two SONAME-3 targets (Debian 12,
Ubuntu 24.04, both shipping libfuse3 3.14.0) `coffer mount` prints
`fuse: warning: library too old, some operations may not work` - cosmetic,
every operation in the test cycle (including the ones the warning calls
out) works correctly regardless; it doesn't appear on the newer SONAME-4
targets (Debian 13's 3.17.2, Ubuntu 26.04's 3.18.2).

## Files in this repo

- `src/` — the FUSE filesystem + CLI implementation
- `Cargo.toml` / `Cargo.lock` — dependencies and the `cargo-deb` packaging
  metadata
- `Makefile` — `make build` / `make install` / `make man`
- `packaging/` — the `coffer(1)` man page and the per-distro `.deb` build
  scripts (see **Packaging** above)
- `.github/workflows/release.yml` — builds a `.deb` per target distro and
  publishes it to GitHub Releases
