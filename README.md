# cryptc

A growable, encrypted, single-file container you create and mount **as a
normal user — no root, no sudo**. Backed by SQLCipher (statically bundled,
no runtime dependency on a system SQLCipher package) and mounted via FUSE.
Once mounted it behaves like an ordinary directory: `cp`, `mv`, `rsync`,
file managers, all work normally.

```
cryptc create vault.cryptc
cryptc mount  vault.cryptc ~/vault
cp -r ~/Documents/secret-stuff ~/vault/
cryptc umount ~/vault
```

## Install

**Prebuilt `.deb`** — one package per target distro (Debian 12/13, Ubuntu
24.04/26.04), each built natively in that distro (see **Packaging** below
for why there's one per distro rather than a single universal package):

```bash
sudo apt-get install ./cryptc_*_<debian12|debian13|ubuntu2404|ubuntu2604>_amd64.deb
```

Grab the matching `.deb` for your distro from the
[Releases page](https://github.com/rtulke/cryptc/releases), or build them
all yourself (see **Packaging** below).

**From source:**

```bash
./setup.sh          # Debian 12/13, Ubuntu 24.04/26.04 - see below
make build           # after setup.sh once, this is all you need
sudo make install    # optional: installs to /usr/local/bin/cryptc
                      # + the cryptc(1) man page
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

`vault.cryptc` is a single [SQLCipher](https://www.zetetic.net/sqlcipher/)
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
a hard ceiling (e.g. `cryptc create vault.cryptc --max-size 10G`); writes
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
   mid-write, and `cryptc check` afterwards reported no corruption with
   every previously-written file byte-for-byte intact.
2. **Per-page HMAC integrity.** SQLCipher attaches an HMAC to every 4 KiB
   page. `cryptc check <file>` runs `PRAGMA cipher_integrity_check` (catches
   tampering/bit-rot at the encryption layer) and `PRAGMA integrity_check`
   (catches structural corruption) without modifying anything. Note: SQLCipher
   itself has a confirmed upstream bug where `cipher_integrity_check`
   misreports every page past the 4GB mark as HMAC-failed on containers
   larger than that (reproduced independently of this project, across two
   separate SQLCipher builds - not something we can fix here). `cryptc check`
   detects this specific false-positive pattern and treats `integrity_check`
   (which actually decrypts and verifies every page) as authoritative, so it
   won't cry wolf on a large, healthy container - while still correctly
   failing on real corruption anywhere in the file.
3. **Live, consistent backups.** `cryptc backup <file> <dest>` uses
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
well. Change the password any time with `cryptc passwd <file>`.

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
cryptc create vault.cryptc
cryptc create vault.cryptc --max-size 20G     # optional hard ceiling

# Mount / unmount (as yourself, no sudo)
cryptc mount  vault.cryptc ~/vault
cryptc umount ~/vault

# Maintenance
cryptc info   vault.cryptc      # file/dir counts, size on disk vs. logical data
cryptc check  vault.cryptc      # read-only integrity check (HMAC + structural)
cryptc backup vault.cryptc vault.bak.cryptc   # consistent copy, safe while mounted
cryptc passwd vault.cryptc      # change the password
```

## Design notes

- Inode numbers are `AUTOINCREMENT` (never reused), avoiding a FUSE
  inode-reuse hazard.
- Every mutating FUSE call (write, mkdir, rename, ...) is wrapped in one
  explicit SQLite transaction and committed once, so a single filesystem
  operation is always all-or-nothing.
- `mount` daemonizes by default (returns control to the shell immediately);
  pass `--foreground` to keep it attached for debugging.
- Password prompts mask input on a real terminal; if stdin isn't a TTY
  (piping, scripting), it falls back to a visible plain-text read.
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

While verifying integrity on the 12GB benchmark container, `cryptc check`
reported hundreds of thousands of "corrupt" pages, all starting at exactly
page 1,048,577 - which, at 4096 bytes/page, is precisely the 4GB mark
(2^32 bytes). That's too precise to be real corruption, so it was run down:

- Reproduced with a **30-line program using nothing but `rusqlite` +
  SQLCipher** - no FUSE, no cryptc schema, just a table with a BLOB column,
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
data itself is fine. `cryptc check` detects this specific pattern - a page
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
of writing still bundles 4.14.0 (predates the fix), so `cryptc check`'s
workaround above remains necessary until that crate updates its vendored
copy.

## Known limitations (honest scope)

- **Single mounter at a time.** The FUSE loop is intentionally
  single-threaded to keep the SQLite access pattern simple and correct.
  Don't mount the same container twice concurrently.
- **No `default_permissions` enforcement.** Whoever can supply the
  password gets full read/write access to everything inside; Unix
  permission bits are stored and reported but not enforced. This matches
  the personal-container use case (VeraCrypt-style), not a multi-user
  shared filesystem.
- **Large files are fine, but not optimized for huge ones.** Reads/writes
  go through per-128KiB-block SQL statements. Great for documents, photos,
  typical file collections; you would want a different design (e.g. a
  dedicated blob store) if you're routinely storing many multi-GB files.
- **No shrink/compaction command yet.** Deleting files frees rows inside
  the database but the container file itself doesn't shrink automatically
  (SQLite reuses that freed space for future writes though). Run `VACUUM`
  via the `sqlcipher` command-line shell manually if you need to reclaim
  disk space after large deletions, or add a `cryptc compact` subcommand.
- **`cipher_integrity_check` false positives past 4GB.** See above -
  `cryptc check` already works around this, but be aware if you ever run
  the raw `PRAGMA cipher_integrity_check` yourself against a large
  container.

## Packaging

`packaging/build-deb.sh` builds **one `.deb` per target distro**, each
natively inside that distro's own container, via
[cargo-deb](https://github.com/kornelski/cargo-deb). Output goes to
`dist/cryptc_<version>_<debian12|debian13|ubuntu2404|ubuntu2604>_amd64.deb`.
Every package also installs the `cryptc(1)` man page
(`packaging/cryptc.1`, gzipped by `make man` before `cargo deb` runs - see
the Makefile) to `/usr/share/man/man1/`.

```bash
packaging/build-deb.sh          # -> dist/cryptc_*_<id>_amd64.deb (all 4)
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
Ubuntu 24.04, both shipping libfuse3 3.14.0) `cryptc mount` prints
`fuse: warning: library too old, some operations may not work` - cosmetic,
every operation in the test cycle (including the ones the warning calls
out) works correctly regardless; it doesn't appear on the newer SONAME-4
targets (Debian 13's 3.17.2, Ubuntu 26.04's 3.18.2).

## Files in this repo

- `src/` — the FUSE filesystem + CLI implementation
- `Cargo.toml` / `Cargo.lock` — dependencies and the `cargo-deb` packaging
  metadata
- `Makefile` — `make build` / `make install` / `make man`
- `packaging/` — the `cryptc(1)` man page and the per-distro `.deb` build
  scripts (see **Packaging** above)
- `.github/workflows/release.yml` — builds a `.deb` per target distro and
  publishes it to GitHub Releases
