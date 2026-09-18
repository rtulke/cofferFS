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
///
/// The trigger keeps the table consistent no matter which version deletes
/// an inode: it lives in the schema, so a `DELETE FROM inodes` issued by a
/// coffer from before the table existed drops the inode's attributes too.
/// Without it an older version's unlink would leave orphan rows behind.
const XATTRS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS xattrs (
    ino   INTEGER NOT NULL,
    name  TEXT NOT NULL,
    value BLOB NOT NULL,
    PRIMARY KEY (ino, name)
) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS xattrs_gc AFTER DELETE ON inodes
BEGIN
    DELETE FROM xattrs WHERE ino = OLD.ino;
END;
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

/// SQLCipher wires up its own diagnostics the first time it initialises
/// (sqlcipher_extra_init: default level WARN, target stderr). A wrong
/// password then dumps three `ERROR CORE ... hmac check failed` lines onto
/// the terminal right before coffer's own "wrong password" message. The
/// level is process-wide, so the first connection's PRAGMA is what counts;
/// it is issued on every keyed connection anyway so no entry point can
/// miss it. Nothing is lost: every condition SQLCipher logs at ERROR is
/// also surfaced as an SQLite error code, which coffer already reports.
fn silence_sqlcipher_log(con: &Connection) -> rusqlite::Result<()> {
    con.execute_batch("PRAGMA cipher_log_level = NONE;")
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
    silence_sqlcipher_log(&con)?;
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
    silence_sqlcipher_log(&con)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_container() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.coffer");
        create_container(&path, "test-password", 0).unwrap();
        (dir, path)
    }

    /// The xattrs_gc trigger is what keeps an older coffer - one that does
    /// not know the xattrs table - from leaving orphan rows when it deletes
    /// an inode with a plain `DELETE FROM inodes`. Exercised here exactly
    /// that way, without going through fs.rs.
    #[test]
    fn deleting_an_inode_row_drops_its_xattrs() {
        let (_dir, path) = temp_container();
        let con = open_db(&path, "test-password", false).unwrap();
        con.execute(
            "INSERT INTO inodes (parent, name, kind, mode, uid, gid, size, atime, mtime, ctime) \
             VALUES (1, 'f', ?1, 420, 0, 0, 0, 0.0, 0.0, 0.0)",
            rusqlite::params![KIND_FILE],
        )
        .unwrap();
        let ino: i64 = con.query_row("SELECT ino FROM inodes WHERE name='f'", [], |r| r.get(0)).unwrap();
        con.execute(
            "INSERT INTO xattrs (ino, name, value) VALUES (?1, 'user.a', x'01'), (?1, 'user.b', x'02')",
            rusqlite::params![ino],
        )
        .unwrap();
        let count = |c: &Connection| -> i64 {
            c.query_row("SELECT count(*) FROM xattrs WHERE ino=?1", rusqlite::params![ino], |r| r.get(0)).unwrap()
        };
        assert_eq!(count(&con), 2);
        con.execute("DELETE FROM inodes WHERE ino=?1", rusqlite::params![ino]).unwrap();
        assert_eq!(count(&con), 0, "trigger must remove the inode's xattr rows");
    }

    /// A container created before the xattrs table existed gets it on the
    /// first writable open, and never on a read-only one.
    #[test]
    fn xattrs_table_is_added_on_writable_open_only() {
        let (_dir, path) = temp_container();
        {
            let con = open_db(&path, "test-password", false).unwrap();
            con.execute_batch("DROP TRIGGER xattrs_gc; DROP TABLE xattrs;").unwrap();
            assert!(!has_xattrs(&con));
        }
        {
            let con = open_db(&path, "test-password", true).unwrap();
            assert!(!has_xattrs(&con), "read-only open must not add the table");
        }
        {
            let con = open_db(&path, "test-password", false).unwrap();
            assert!(has_xattrs(&con), "writable open adds the table");
            let trigger: i64 = con
                .query_row("SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name='xattrs_gc'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(trigger, 1);
        }
    }

    #[test]
    fn wrong_password_and_newer_schema_are_refused() {
        let (_dir, path) = temp_container();
        assert!(open_db(&path, "not-the-password", true).is_err());
        {
            let con = open_db(&path, "test-password", false).unwrap();
            con.execute("UPDATE meta SET value='99' WHERE key='schema_version'", []).unwrap();
        }
        let err = open_db(&path, "test-password", true).unwrap_err().to_string();
        assert!(err.contains("format version 99"), "{err}");
    }
}
