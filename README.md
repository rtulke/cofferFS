# cryptc

A single-file encrypted container you create and mount **as a normal user
— no root, no sudo**. Once mounted it behaves like an ordinary directory:
`cp`, `mv`, `rsync`, file managers, all work normally.

```
cryptc create vault.cryptc
cryptc mount  vault.cryptc ~/vault
cp -r ~/Documents/secret-stuff ~/vault/
cryptc umount ~/vault
```

## Two implementations, same format

| | Use this for | Docs |
|---|---|---|
| **Rust** (`rust/`) | Production / any real amount of data. Compiled, faster, packaged as a `.deb` for Debian 12/13 and Ubuntu 24.04/26.04. | [rust/README.md](rust/README.md) |
| **Python** (this file, `cryptc` at repo root) | The original reference implementation - quicker to read/hack on, no compiler needed. | you're reading it |

Both read/write the identical container format and share the same CLI
(`create`/`mount`/`umount`/`check`/`backup`/`passwd`/`info`) and the same
crash-safety/encryption design described below - the sections on *how it
works*, *why it doesn't destroy your data*, and *known limitations* apply to
both. Everything past **Requirements / build environment** below is
Python-specific; see [rust/README.md](rust/README.md) for the Rust build,
benchmarks, and packaging instructions.

## How it works

`vault.cryptc` is a single [SQLCipher](https://www.zetetic.net/sqlcipher/)
(encrypted SQLite) database. Directories and files are rows in that
database; file content is stored in 128 KiB chunks. Mounting is done via
[FUSE](https://github.com/libfuse/libfuse) - the Python implementation uses
the `fusepy` bindings, the Rust port uses the `fuser` crate - which is why
no root privileges are required either way: FUSE mounts are owned by, and
only accessible to, the user who created them.

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
   larger than that (reproduced independently of this project, across three
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
takes well under a second - our `readdir()` reports a real file type per
entry, so tools like `find` don't need an extra syscall per file just to
answer `-type f`).

Writing at scale used to be the weak point: without explicit FUSE mount
options, the kernel capped each `write()` request at 4 KiB, so one ~126KB
file arrived as ~31 separate FUSE calls - and since every write committed
its own transaction, that was ~31 commits instead of 1. This is fixed
(`big_writes`/`max_write` now negotiate a request size matching the storage
block size). Benchmarked end to end: populating 95,000 files (~12GB, mixed
sizes averaging ~126KB) took about 6.7 minutes (~236 files/s, ~30MB/s),
holding a flat rate rather than degrading as the container grew. See the
[Rust port's README](rust/README.md) for the same benchmark against the
compiled version, which is faster still.

## Requirements / build environment (Python)

For the Rust build, see [rust/README.md](rust/README.md) instead - it also
covers the prebuilt `.deb`.

- Linux with FUSE 3 (`fuse3` package) — this was built and tested on
  Ubuntu 24.04. macOS works via [macFUSE](https://osxfuse.github.io/)
  (`brew install --cask macfuse`, one-time security approval required).
- Python 3.9+

```bash
./setup.sh                 # installs fuse3 (via apt/dnf/brew) + creates .venv
source .venv/bin/activate
```

`setup.sh` is safe to re-run; it just installs what's missing.

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

## Known limitations (honest scope)

- **Single mounter at a time.** The FUSE loop is intentionally
  single-threaded (`nothreads=True`) to keep the SQLite access pattern
  simple and correct. Don't mount the same container twice concurrently.
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
  (SQLite reuses that freed space for future writes though). Run
  `VACUUM` via `sqlite3`/`sqlcipher3` manually if you need to reclaim disk
  space after large deletions, or add a `cryptc compact` subcommand.
- **`cipher_integrity_check` false positives past 4GB.** An upstream
  SQLCipher bug (see above) misreports pages beyond the 4GB mark as
  HMAC-failed on large containers. `cryptc check` already works around this
  by trusting `integrity_check` instead, but be aware if you ever run the
  raw `PRAGMA cipher_integrity_check` yourself against a large container.

## Files in this repo

- `cryptc` — the Python CLI + FUSE filesystem implementation (single file)
- `requirements.txt` — `fusepy`, `sqlcipher3-binary`
- `setup.sh` — bootstraps the system packages + Python virtualenv
- `rust/` — the compiled Rust port: source, `Makefile`, `setup.sh`, and
  `.deb` packaging (see [rust/README.md](rust/README.md))
- `upstream-sqlcipher-bug/` — reproduction and write-up for an upstream
  SQLCipher bug found while building this (`cipher_integrity_check` false
  positives on databases past 4GB)
