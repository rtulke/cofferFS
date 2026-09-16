use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use zeroize::Zeroizing;

pub const BLOCK_SIZE: i64 = 128 * 1024;
// Schema 1 with additive extras: the `xattrs` table (0.1.3) is created when
// missing and simply ignored by older versions, which only ever touch
// `meta`, `inodes` and `data` - so containers stay usable in both
// directions. Only a change to those three tables bumps this number.
pub const SCHEMA_VERSION: &str = "1";
pub const ROOT_INO: u64 = 1;

pub const KIND_DIR: i64 = 0;
pub const KIND_FILE: i64 = 1;
pub const KIND_SYMLINK: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);
CREATE TABLE inodes (
    ino            INTEGER PRIMARY KEY AUTOINCREMENT,
    parent         INTEGER NOT NULL,
    name           TEXT NOT NULL,
    kind           INTEGER NOT NULL,
    mode           INTEGER NOT NULL,
    uid            INTEGER NOT NULL,
    gid            INTEGER NOT NULL,
    size           INTEGER NOT NULL DEFAULT 0,
    atime          REAL NOT NULL,
    mtime          REAL NOT NULL,
    ctime          REAL NOT NULL,
    symlink_target TEXT,
    UNIQUE(parent, name)
);
CREATE INDEX idx_inodes_parent ON inodes(parent);
CREATE TABLE data (
    ino      INTEGER NOT NULL,
    block_no INTEGER NOT NULL,
    content  BLOB NOT NULL,
    PRIMARY KEY (ino, block_no)
) WITHOUT ROWID;
";

/// Extended attributes, one row per (inode, name). Kept separate from the
/// schema above so it can be added to existing containers on open (see
/// `open_db`): `CREATE TABLE IF NOT EXISTS` is a no-op on a container that
/// already has it and an additive change on one from before 0.1.3.
const XATTRS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS xattrs (
    ino   INTEGER NOT NULL,
    name  TEXT NOT NULL,
    value BLOB NOT NULL,
    PRIMARY KEY (ino, name)
) WITHOUT ROWID;
";

/// Whether the container has the `xattrs` table. Always true after a
/// writable `open_db`; false for a read-only open of a container created by
/// a version before 0.1.3 and never since written by a newer one.
pub fn has_xattrs(con: &Connection) -> bool {
    con.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='xattrs'",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

/// PRAGMA statements don't support bound (?) parameters in SQLite, so the
/// password has to be embedded as a quoted string literal. Escape any
/// embedded single quotes the standard SQL way (' -> '').
pub fn pragma_key_sql(pragma: &str, value: &str) -> Zeroizing<String> {
    let escaped = Zeroizing::new(value.replace('\'', "''"));
    Zeroizing::new(format!("PRAGMA {pragma} = '{}'", *escaped))
}

/// Where SQLite puts the temporary database that VACUUM (see `vacuum`)
/// builds the compacted copy in: next to the container. Call once, early,
/// before any other thread exists - SQLite reads SQLITE_TMPDIR with
/// getenv() at the moment it creates the file, and mutating the
/// environment while other threads run is a data race. Without this the
/// default temp directory applies, which is /tmp on many distros - and
/// /tmp is frequently a tmpfs, i.e. RAM again, defeating the purpose.
pub fn set_temp_dir_beside(container_path: &Path) {
    if let Some(dir) = container_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::env::set_var("SQLITE_TMPDIR", dir);
    }
}

/// VACUUM rebuilds the whole database into a temporary database and then
/// copies it back. SQLite is compiled with SQLITE_TEMP_STORE=2 here (the
/// rusqlite bundled default), which keeps temporary databases *in memory*
/// unless told otherwise - measured: compacting a 415 MB container peaked
/// at 372 MB RSS, so a 10 GB container would need 10 GB of RAM and, from
/// the idle watcher, take the mount daemon down with it. `temp_store =
/// FILE` sends the copy to disk instead (see `set_temp_dir_beside`).
///
/// The copy stays encrypted: VACUUM attaches its temporary database
/// without a KEY, and SQLCipher's attach hook then keys it with the main
/// database's key (attachFunc, `case SQLITE_NULL`) - no plaintext touches
/// the disk.
///
/// The checkpoint is explicit because on a long-lived connection (the
/// idle watcher's) VACUUM alone only lands in the WAL and the file on
/// disk never shrinks.
pub fn vacuum(con: &Connection) -> rusqlite::Result<()> {
    con.execute_batch("PRAGMA temp_store = FILE; VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
}

pub fn create_container(path: &Path, password: &str, max_size: u64) -> Result<()> {
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }

    let con = Connection::open(path).context("creating container file")?;
    con.execute_batch(&pragma_key_sql("key", password))?;
    con.execute_batch("PRAGMA cipher_page_size = 4096;")?;
    con.execute_batch(SCHEMA)?;
    con.execute_batch(XATTRS_SCHEMA)?;

    let now = now_secs();
    con.execute(
        "INSERT INTO inodes (ino, parent, name, kind, mode, uid, gid, size, atime, mtime, ctime) \
         VALUES (1, 0, '', ?1, ?2, ?3, ?4, 0, ?5, ?5, ?5)",
        rusqlite::params![
            KIND_DIR,
            (libc::S_IFDIR | 0o700) as i64,
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
            now,
        ],
    )?;

    let meta: [(&str, String); 4] = [
        ("schema_version", SCHEMA_VERSION.to_string()),
        ("created_at", now.to_string()),
        ("block_size", BLOCK_SIZE.to_string()),
        ("max_size", max_size.to_string()),
    ];
    for (k, v) in meta {
        con.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![k, v],
        )?;
    }
    con.execute_batch("PRAGMA journal_mode = WAL;")?;
    drop(con);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn open_db(path: &Path, password: &str, readonly: bool) -> Result<Connection> {
    if !path.is_file() {
        bail!("{} not found", path.display());
    }
    let uri = format!(
        "file:{}{}",
        path.canonicalize()?.display(),
        if readonly { "?mode=ro" } else { "" }
    );
    let flags = if readonly {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI
    } else {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI
    };
    let con = Connection::open_with_flags(&uri, flags)?;
    con.execute_batch(&pragma_key_sql("key", password))?;

    let check: rusqlite::Result<String> = con.query_row(
        "SELECT value FROM meta WHERE key='schema_version'",
        [],
        |r| r.get(0),
    );
    let schema = match check {
        Ok(v) => v,
        Err(_) => bail!("wrong password, or the file is not a coffer container"),
    };
    // Compared, not just read: a container written by a newer coffer with
    // a changed schema must be refused with a clear message, not opened
    // and then mishandled (or silently written to) by this older one.
    if schema != SCHEMA_VERSION {
        bail!(
            "{} uses container format version {schema}, this coffer understands version {SCHEMA_VERSION} - upgrade coffer",
            path.display()
        );
    }

    if !readonly {
        con.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = OFF;")?;
        // Additive upgrade for containers from before 0.1.3; a no-op after
        // the first time. Not done on read-only opens, which then just
        // report no extended attributes.
        con.execute_batch(XATTRS_SCHEMA)?;
    }
    // cache_size in KiB (negative = KiB rather than page count): keep far more
    // decrypted pages hot than SQLite's tiny ~2MB default, since every page miss
    // means SQLCipher has to re-decrypt+HMAC-verify that page on the next touch.
    // Applies to read-only connections too - a read-only mount reads just as
    // much as a writable one.
    con.execute_batch("PRAGMA cache_size = -131072;")?;
    con.set_prepared_statement_cache_capacity(64);
    Ok(con)
}

pub fn read_max_size(con: &Connection) -> u64 {
    con.query_row("SELECT value FROM meta WHERE key='max_size'", [], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .and_then(|s| s.parse().ok())
    .unwrap_or(0)
}

pub fn now_secs() -> f64 {
    // A clock set before 1970 (dead CMOS battery, broken NTP at boot) would
    // otherwise panic here - and since this runs on nearly every FUSE call
    // (touch(), every mtime/ctime update), that's a repeated full-process
    // abort on every single filesystem operation, not just a one-off. 0.0
    // is a harmless fallback: it just means timestamps look like the epoch
    // until the clock is fixed, not a crash.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Exclusive advisory lock guarding the *write-intent* operations (mount,
/// passwd, compact) against each other - two live writers on the same
/// container is the actual danger this project cares about. Deliberately
/// not used by check/backup/info: WAL mode already gives readers a safe,
/// consistent view alongside an active writer (backup's online-backup API
/// is documented to rely on exactly that), so locking them out would only
/// get in the way of something that's already safe.
///
/// Held on a separate fd from the one SQLite itself uses (flock() and
/// SQLite's own fcntl() record locks don't interact), released
/// automatically by the kernel the moment every fd referencing it closes -
/// including on a crash or kill -9 - so there's no stale-lock case to
/// handle, unlike a PID file.
pub fn lock_exclusive(path: &Path) -> Result<File> {
    // Opened read-write, not read-only: local filesystems' flock() ignores
    // the fd's open mode, but on NFS the kernel emulates flock() via
    // byte-range locks and rejects LOCK_EX on a read-only fd with EBADF.
    // All three callers (mount/passwd/compact) already need write access to
    // the container right after this anyway, so this costs nothing locally.
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let ret = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            // On a network filesystem (NFS) this lock is visible across
            // hosts, so the holder can just as well be a mount left running
            // on another machine - worth saying, since nothing on *this*
            // host will show it.
            bail!(
                "{} is already in use by another coffer process (mounted, or a passwd/compact in \
progress). If the container lives on a network filesystem, that process may be running on \
another host.",
                path.display()
            );
        }
        return Err(err).context("acquiring lock");
    }
    Ok(f)
}
