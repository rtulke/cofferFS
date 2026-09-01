use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::Path;

pub const BLOCK_SIZE: i64 = 128 * 1024;
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

/// PRAGMA statements don't support bound (?) parameters in SQLite, so the
/// password has to be embedded as a quoted string literal. Escape any
/// embedded single quotes the standard SQL way (' -> '').
pub fn pragma_key_sql(pragma: &str, value: &str) -> String {
    let escaped = value.replace('\'', "''");
    format!("PRAGMA {pragma} = '{escaped}'")
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
    if check.is_err() {
        bail!("wrong password, or the file is not a coffer container");
    }

    if !readonly {
        // cache_size in KiB (negative = KiB rather than page count): keep far more
        // decrypted pages hot than SQLite's tiny ~2MB default, since every page miss
        // means SQLCipher has to re-decrypt+HMAC-verify that page on the next touch.
        con.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; \
             PRAGMA foreign_keys = OFF; PRAGMA cache_size = -131072;",
        )?;
    }
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
            bail!(
                "{} is already in use by another coffer process (mounted, or a passwd/compact in progress)",
                path.display()
            );
        }
        return Err(err).context("acquiring lock");
    }
    Ok(f)
}
