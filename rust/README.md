# cryptc (Rust)

A growable, encrypted, single-file container you create and mount **as a
normal user — no root, no sudo**. Backed by SQLCipher (statically bundled,
no runtime dependency on a system SQLCipher package) and mounted via FUSE.
This is the compiled, packaged version of cryptc; a
[Python reference implementation](../cryptc) with identical behavior and
on-disk format lives in the parent directory (both read/write the same
container file, and either can be used to `check`/`backup`/`mount` a
container the other created).

```
cryptc create vault.cryptc
cryptc mount  vault.cryptc ~/vault
cp -r ~/Documents/secret-stuff ~/vault/
cryptc umount ~/vault
```

## Install

**Prebuilt `.deb`** — one package per target distro (Debian 12/13, Ubuntu
24.04/26.04), each built natively in that distro (see **Packaging** for
why there's one per distro rather than a single universal package):

```bash
sudo apt-get install ./cryptc_*_<debian12|debian13|ubuntu2404|ubuntu2604>_amd64.deb
```

Grab the matching `.deb` for your distro from the
[Releases page](../../releases), or build them all yourself (see
**Packaging** below).

**From source:**

```bash
./setup.sh          # Debian 12/13, Ubuntu 24.04/26.04 - see below
make build           # after setup.sh once, this is all you need
sudo make install    # optional: installs to /usr/local/bin/cryptc
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
`cargo build --release`.

## Why a rewrite, and why Rust over C

The Python prototype proved the design (SQLCipher-encrypted SQLite as a
growable, user-mountable FUSE container) and its crash-safety claims. A
compiled rewrite removes the Python/FUSE-binding overhead. Rust was chosen
over C specifically because:

- `fuser` (FUSE bindings) and `rusqlite` (with the `sqlcipher` feature) map
  closely to the Python code's structure (`fusepy`'s `Operations` class →
  the `Filesystem` trait; `sqlcipher3`'s `con.execute(...)` → `rusqlite`
  calls), so the port was close to mechanical rather than a redesign.
- The compiler's memory safety removes an entire class of bugs (leaks,
  use-after-free, buffer overflows in the block-storage code) that hand-
  rolled C memory management would risk introducing — for a tool whose
  whole point is *not* corrupting your data, that risk isn't worth taking.
- Both `libfuse3-dev` and `libsqlcipher-dev` are readily available via apt
  either way, so C had no build-environment advantage here.

## Differences from the Python version

- Inode numbers are `AUTOINCREMENT` (never reused), avoiding a FUSE
  inode-reuse hazard the Python prototype didn't need to worry about at its
  scale.
- Every mutating FUSE call (write, mkdir, rename, ...) is wrapped in one
  explicit SQLite transaction and committed once, so a single filesystem
  operation is always all-or-nothing — matching, and making more explicit,
  the crash-safety property demonstrated in the Python version.
- `mount` daemonizes by default (returns control to the shell immediately,
  like the Python version's `fusepy` default); pass `--foreground` to keep
  it attached for debugging.
- Password prompts mask input on a real terminal; if stdin isn't a TTY
  (piping, scripting), it falls back to a visible plain-text read, same
  spirit as Python's `getpass` fallback.

Everything else — the schema, the block size, the CLI subcommands
(`create`/`mount`/`umount`/`check`/`backup`/`passwd`/`info`), the growable-
by-default design, and the WAL/SQLCipher crash-safety story — is identical
to what's documented in the [parent README](../README.md).

## Verified while building this

Same test pass as the Python version, against the compiled binary: created
a container, mounted it as a non-root user, wrote 1000 small files plus a
20MB file (checksum-verified), unmounted and remounted to confirm
persistence, rejected a wrong password, ran `check`/`backup`/`passwd`
successfully, and hard-`kill -9`'d the mount process mid-write — the
container stayed structurally intact (`check` clean) with every
previously-written byte recoverable afterward.

## Performance at scale (95,000 files, ~12GB)

Both implementations were benchmarked end to end at a realistic scale
(95,000 files, mixed sizes averaging ~126KB, ~12GB total) against a plain
ext4 baseline for context:

| | population (write) | `find -type f \| wc -l` |
|---|---|---|
| ext4 (baseline) | 23.6s (4027 files/s, 509MB/s) | 0.05s |
| Python (fixed) | 402.8s (236 files/s, 30MB/s) | ~5s (extrapolated) |
| Rust (before tuning) | 463.5s (205 files/s, 26MB/s) | 0.94s |
| **Rust (tuned)** | **365.3s (260 files/s, 33MB/s)** | 0.94s |

Reading/listing was never the bottleneck at any scale tested. Two real
issues turned up while chasing write throughput at this scale, both fixed:

1. **Python: FUSE was chunking every write into ~4KB pieces.** Without
   `big_writes`/`max_write` mount options, the kernel capped each `write()`
   request at 4KB, so a ~126KB file arrived as ~31 separate FUSE calls -
   and since each write committed its own transaction, that meant ~31
   commits instead of 1. Profiling with `cProfile` on the live FUSE process
   (in-process, since `ptrace`-based tools like `strace`/`py-spy` aren't
   always available) showed 67% of total time inside `Connection.commit()`
   alone. Fixed in the Python version by passing `big_writes=True` and
   `max_write`/`max_read` matching the storage block size - a ~16x
   throughput improvement (23 files/s -> 369 files/s in isolated testing).
   `fuser` (this Rust port's FUSE binding) already negotiates a large
   `max_write` by default, so Rust never had this problem.
2. **Rust: every FUSE call re-parsed its SQL.** `fs.rs` now uses
   `Connection::prepare_cached` throughout instead of `execute`/`prepare`,
   and `PRAGMA cache_size` is raised from SQLite's ~2MB default to 128MB
   (SQLCipher has to re-decrypt+HMAC-verify a page every time it's evicted
   from cache and re-read, so a bigger cache means less redundant crypto
   work as the container grows). Together these cut the full 95,000-file/
   12GB run from 463s to 365s (~27% faster).

**Methodology note:** benchmark runs must be isolated. An early comparison
that ran the Python and Rust population scripts concurrently on the same
4-core box showed Python collapsing to ~10 files/s - that was resource
contention with the concurrently-running Rust benchmark, not a real
Python-specific cliff. Once run alone, Python held a flat ~23 files/s
(pre-fix) / ~236-369 files/s (post-fix) for the whole run. Always benchmark
one mount at a time.

## Known upstream SQLCipher bug (not ours, but worth knowing about)

While verifying integrity on the 12GB benchmark container, `cryptc check`
reported hundreds of thousands of "corrupt" pages, all starting at exactly
page 1,048,577 - which, at 4096 bytes/page, is precisely the 4GB mark
(2^32 bytes). That's too precise to be real corruption, so it was run down:

- Reproduced with a **30-line program using nothing but `rusqlite` +
  SQLCipher** - no FUSE, no cryptc schema, just a table with a BLOB column,
  written to past 4GB and integrity-checked. Same failure, same exact page.
- Reproduced identically across **three independent SQLCipher builds**:
  Ubuntu's `libsqlcipher-dev` package, Python's `sqlcipher3-binary` PyPI
  wheel (a separately-compiled, bundled build), and a from-source build via
  `bundled-sqlcipher-vendored-openssl`. All three fail at the exact same
  page number.
- Meanwhile `PRAGMA integrity_check` - which actually walks and decrypts
  every page to verify the B-tree structure, rather than just recomputing
  HMACs - reported `ok` every time. Files written well past the 4GB mark
  (including the very last file in a 12GB container) read back with
  correct, repeatable checksums.

Conclusion: `PRAGMA cipher_integrity_check` has a real bug in its own page-
iteration logic for databases past 4GB, independent of this project. The
data itself is fine. `cryptc check` (both the Python and Rust versions) now
detects this specific pattern - a page number past the 4GB boundary flagged
by `cipher_integrity_check` while `integrity_check` passes cleanly - and
reports the container as healthy with an explanatory note, while still
failing loudly on genuine corruption anywhere in the file (verified with a
deliberate single-byte flip in a small container: still caught, still exits
non-zero).

## Packaging

`packaging/build-deb.sh` builds **one `.deb` per target distro**, each
natively inside that distro's own container, via
[cargo-deb](https://github.com/kornelski/cargo-deb). Output goes to
`dist/cryptc_<version>_<debian12|debian13|ubuntu2404|ubuntu2604>_amd64.deb`.

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
natively per distro (matching the pattern already used for this project's
other packaging pipelines) sidesteps the whole question - each package
just depends on whatever that distro actually ships.

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
