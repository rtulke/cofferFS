use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use zeroize::Zeroizing;

pub const BLOCK_SIZE: i64 = 128 * 1024;
// Version 2 (0.2.0) moved names out of `inodes` into their own `dentries`
// table so one inode can carry several names - hard links. That is a
// change to the core tables, so unlike the additive `xattrs` table it
// bumps the number: a container is migrated on its first writable open
// (see `migrate_to_v2`) and is then no longer readable by coffer 0.1.x,
// which refuses it with a clear message rather than misreading it.
pub const SCHEMA_VERSION: &str = "2";
const SCHEMA_VERSION_V1: &str = "1";
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
    kind           INTEGER NOT NULL,
    mode           INTEGER NOT NULL,
    uid            INTEGER NOT NULL,
    gid            INTEGER NOT NULL,
    size           INTEGER NOT NULL DEFAULT 0,
    atime          REAL NOT NULL,
    mtime          REAL NOT NULL,
    ctime          REAL NOT NULL,
    symlink_target TEXT
);
CREATE TABLE dentries (
    parent INTEGER NOT NULL,
    name   TEXT NOT NULL,
    ino    INTEGER NOT NULL,
    PRIMARY KEY (parent, name)
) WITHOUT ROWID;
CREATE INDEX idx_dentries_ino ON dentries(ino);
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

/// The container's format version, as stored in `meta`.
pub fn schema_version(con: &Connection) -> Option<String> {
    con.query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0)).ok()
}

/// Version 1 kept a directory entry's parent and name in `inodes` itself,
/// one row per name, which is why a file could have exactly one. Version 2
/// moves them into `dentries`, so several entries may point at the same
/// inode.
///
/// The whole thing is one transaction: either the container comes out as
/// version 2 or it is untouched, even on a crash or a full disk halfway
/// through. Dropping `inodes` takes the `xattrs_gc` trigger with it, so
/// the caller runs the xattrs schema again afterwards to put it back.
///
/// `AUTOINCREMENT` survives, but not by itself. Copying the rows with
/// their explicit `ino` only raises the new table's sequence to the
/// highest *surviving* inode, and `DROP TABLE` deletes the old table's
/// `sqlite_sequence` row along with it - so in any container whose
/// most recently created file had since been deleted, the high-water mark
/// would drop and the next file would be handed a number that had already
/// been used. The explicit carry-over below prevents that: inode numbers
/// keep going up and are never reused, an invariant FUSE depends on.
fn migrate_to_v2(con: &Connection) -> Result<()> {
    con.execute_batch(
        "BEGIN;
         CREATE TABLE dentries (
             parent INTEGER NOT NULL,
             name   TEXT NOT NULL,
             ino    INTEGER NOT NULL,
             PRIMARY KEY (parent, name)
         ) WITHOUT ROWID;
         INSERT INTO dentries (parent, name, ino)
             SELECT parent, name, ino FROM inodes WHERE ino != 1;
         CREATE INDEX idx_dentries_ino ON dentries(ino);
         CREATE TABLE inodes_v2 (
             ino            INTEGER PRIMARY KEY AUTOINCREMENT,
             kind           INTEGER NOT NULL,
             mode           INTEGER NOT NULL,
             uid            INTEGER NOT NULL,
             gid            INTEGER NOT NULL,
             size           INTEGER NOT NULL DEFAULT 0,
             atime          REAL NOT NULL,
             mtime          REAL NOT NULL,
             ctime          REAL NOT NULL,
             symlink_target TEXT
         );
         INSERT INTO inodes_v2 (ino, kind, mode, uid, gid, size, atime, mtime, ctime, symlink_target)
             SELECT ino, kind, mode, uid, gid, size, atime, mtime, ctime, symlink_target FROM inodes;
         -- Carry the high-water mark over while the old row still exists.
         -- COALESCE, not bare max(): max(x, NULL) is NULL in SQLite, and a
         -- missing source row would otherwise wipe the sequence entirely.
         UPDATE sqlite_sequence
            SET seq = max(seq, COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'inodes'), 0))
          WHERE name = 'inodes_v2';
         DROP TABLE inodes;
         ALTER TABLE inodes_v2 RENAME TO inodes;
         UPDATE meta SET value = '2' WHERE key = 'schema_version';
         COMMIT;",
    )
    .context("migrating the container to format version 2")
}

/// Counts the two logical inconsistencies the schema makes possible but
/// nothing should ever produce: a name in `dentries` pointing at an inode
/// that is not there, and an inode no name points at - whose data blocks
/// then occupy space that nothing can reach or free. Returns
/// `(dangling_names, unreachable_inodes)`.
///
/// Lives here rather than in `cmd_check` because the queries are schema
/// knowledge, and because `cmd_check` itself ends in `process::exit` and
/// cannot be tested.
pub fn find_inconsistencies(con: &Connection) -> rusqlite::Result<(i64, i64)> {
    let dangling = con.query_row(
        "SELECT count(*) FROM dentries d LEFT JOIN inodes i ON i.ino = d.ino WHERE i.ino IS NULL",
        [],
        |r| r.get(0),
    )?;
    let unreachable = con.query_row(
        "SELECT count(*) FROM inodes i WHERE i.ino <> 1 \
         AND NOT EXISTS (SELECT 1 FROM dentries d WHERE d.ino = i.ino)",
        [],
        |r| r.get(0),
    )?;
    Ok((dangling, unreachable))
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
    // The root has no directory entry of its own: nothing names it, and
    // `..` in the root points back at the root (see fs.rs readdir).
    con.execute(
        "INSERT INTO inodes (ino, kind, mode, uid, gid, size, atime, mtime, ctime) \
         VALUES (1, ?1, ?2, ?3, ?4, 0, ?5, ?5, ?5)",
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
    open_db_with(path, password, readonly, false)
}

/// How the mount daemon's connection locks the container (`--locking`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Locking {
    /// Exclusive on a network filesystem, normal everywhere else.
    Auto,
    /// Always exclusive (see `open_db_for_mount`).
    Exclusive,
    /// SQLite's normal per-transaction locking, even on a network filesystem.
    Normal,
}

impl Locking {
    pub fn as_str(self) -> &'static str {
        match self {
            Locking::Auto => "auto",
            Locking::Exclusive => "exclusive",
            Locking::Normal => "normal",
        }
    }

    pub fn parse(s: &str) -> Option<Locking> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Locking::Auto),
            "exclusive" => Some(Locking::Exclusive),
            "normal" => Some(Locking::Normal),
            _ => None,
        }
    }

    /// Whether a mount of `container` should hold its locks exclusively.
    pub fn exclusive_for(self, container: &Path) -> bool {
        match self {
            Locking::Auto => on_network_fs(container),
            Locking::Exclusive => true,
            Locking::Normal => false,
        }
    }
}

/// Whether `path` lives on a filesystem where a lock is a round trip to
/// some other machine. FUSE counts too: sshfs and friends are the common
/// case, and the price of a false positive on a local FUSE filesystem is
/// only that check/info need an unmount there.
#[cfg(target_os = "linux")]
pub fn on_network_fs(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    const NETWORK_FS: &[i64] = &[
        0x6969,      // NFS
        0x517B,      // SMB
        0xFF534D42,  // CIFS
        0xFE534D42,  // SMB2
        0x00C36400,  // Ceph
        0x01021997,  // 9p
        0x6B414653,  // AFS (kAFS)
        0x5346414F,  // AFS (OpenAFS)
        0x01161970,  // GFS2
        0x7461636F,  // OCFS2
        0x0BD00BD0,  // Lustre
        0x65735546,  // FUSE (sshfs, glusterfs, ...)
    ];
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    NETWORK_FS.contains(&(st.f_type as i64))
}

#[cfg(not(target_os = "linux"))]
pub fn on_network_fs(_path: &Path) -> bool {
    false
}

/// The mount daemon's connection when it holds its locks exclusively
/// (`Locking::exclusive_for`): like `open_db`, but in SQLite's EXCLUSIVE
/// locking mode. In the normal mode SQLite takes and drops its fcntl()
/// locks around every single transaction - and every FUSE call is at least
/// one. On a local disk that's a few syscalls. On NFS each one is a LOCK +
/// LOCKU round trip to the server, and the Linux client also drops its page
/// cache for the file on every lock it takes, so the next read goes to the
/// server too. That's hidden while the server grants the open a delegation,
/// but once it recalls it (measured: hours into a mount, unprompted) every
/// FUSE call costs four round trips - `git status` in a small repo went from
/// 0.03s to 1.3s, until a remount got a fresh delegation.
///
/// EXCLUSIVE takes the locks on first access and keeps them until the
/// connection closes, and - because it's set before the first WAL access -
/// SQLite then keeps the WAL index in heap memory instead of an mmap()ed
/// -shm file, so no per-transaction locking is left at all. The price:
/// no other connection can open the container while it's mounted, which is
/// why `backup` goes through the daemon (see ipc.rs) and `check`/`info`
/// ask for an unmount. lock_exclusive() already rules out a second writer,
/// so nothing that was safe before is lost.
///
/// Not for read-only mounts: an exclusive WAL connection needs a write lock
/// on the file, which a read-only fd can't take (SQLITE_IOERR_LOCK), so
/// those keep the normal mode.
pub fn open_db_for_mount(path: &Path, password: &str, readonly: bool, exclusive: bool) -> Result<Connection> {
    open_db_with(path, password, readonly, exclusive && !readonly)
}

fn open_db_with(path: &Path, password: &str, readonly: bool, exclusive: bool) -> Result<Connection> {
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
    if exclusive {
        // Must precede the first read below: only a connection that is
        // already exclusive when it first touches the WAL skips the -shm file.
        con.execute_batch("PRAGMA locking_mode = EXCLUSIVE;")?;
    }

    let check: rusqlite::Result<String> = con.query_row(
        "SELECT value FROM meta WHERE key='schema_version'",
        [],
        |r| r.get(0),
    );
    let schema = match check {
        Ok(v) => v,
        Err(e) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy) => bail!(
            "{} is mounted - the mount holds it exclusively. Unmount it first, or for a copy \
run `coffer backup` on the machine where it is mounted, which goes through the running mount.",
            path.display()
        ),
        Err(_) => bail!("wrong password, or the file is not a coffer container"),
    };
    // A container written by a newer coffer is refused with a clear message
    // rather than misread. The one version below this one is upgraded
    // instead - but that needs write access, so a read-only open leaves it
    // at version 1 and the caller decides: `info`, `check` and `backup`
    // work on either version, a mount does not (see `schema_version`).
    let outdated = schema == SCHEMA_VERSION_V1;
    if schema != SCHEMA_VERSION && !outdated {
        bail!(
            "{} uses container format version {schema}, this coffer understands version {SCHEMA_VERSION} - upgrade coffer",
            path.display()
        );
    }

    if !readonly {
        con.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = OFF;")?;
        if outdated {
            eprintln!(
                "coffer: upgrading {} from container format 1 to 2 (hard links); \
older coffer versions will no longer open it",
                path.display()
            );
            migrate_to_v2(&con)?;
        }
        // Additive upgrade for containers from before 0.1.3; a no-op after
        // the first time. Not done on read-only opens, which then just
        // report no extended attributes. Also puts back the xattrs_gc
        // trigger that the migration above drops along with the old table.
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

/// Raw (still encrypted) first page of `con`'s main database, read through
/// the file descriptor SQLite already has open. Deliberately not by opening
/// the file again: POSIX drops *all* of a process's fcntl() locks on a file
/// the moment it closes *any* descriptor for it, so a plain open/read/close
/// in the mount daemon would silently take away the exclusive mount's locks.
pub fn read_first_page(con: &Connection) -> Result<Vec<u8>> {
    use rusqlite::ffi;
    let mut page = vec![0u8; 4096];
    unsafe {
        let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
        let rc = ffi::sqlite3_file_control(
            con.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_FILE_POINTER,
            &mut file as *mut *mut ffi::sqlite3_file as *mut std::ffi::c_void,
        );
        if rc != ffi::SQLITE_OK || file.is_null() || (*file).pMethods.is_null() {
            bail!("no file handle for the container (SQLite error {rc})");
        }
        let Some(x_read) = (*(*file).pMethods).xRead else {
            bail!("the container's file handle can't read");
        };
        let rc = x_read(file, page.as_mut_ptr() as *mut std::ffi::c_void, page.len() as i32, 0);
        if rc != ffi::SQLITE_OK {
            bail!("reading the container's first page (SQLite error {rc})");
        }
    }
    Ok(page)
}

/// Checks `password` against a container's first page (see
/// `read_first_page`) - for when an exclusive mount holds every lock, so
/// nothing can open the container to find out. Page 1 carries the KDF salt
/// and its own HMAC, and it's always a valid page in the main file (a newer
/// copy may sit in the WAL, but under the same key - a rekey can't happen
/// while mounted). Written to a scratch file in `scratch_dir` and keyed
/// there; a wrong key fails page 1's HMAC check, exactly as a full open
/// would.
pub fn verify_password(page: &[u8], password: &str, scratch_dir: &Path) -> Result<bool> {
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let scratch = Scratch(scratch_dir.join(format!(".verify-{}-{nanos}", std::process::id())));
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new().write(true).create_new(true).mode(0o600).open(&scratch.0)?.write_all(page)?;
    }
    let con = Connection::open(&scratch.0)?;
    silence_sqlcipher_log(&con)?;
    con.execute_batch(&pragma_key_sql("key", password))?;
    con.execute_batch("PRAGMA cipher_page_size = 4096;")?;
    // The header's schema cookie: reading it means decrypting page 1.
    // With the right key page 1 decrypts and SQLite merely finds the
    // database "malformed" (it's one page of many); with the wrong one the
    // HMAC check fails first and it's "not a database".
    match con.query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0)) {
        Ok(_) => Ok(true),
        Err(e) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseCorrupt) => Ok(true),
        Err(e) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::NotADatabase) => Ok(false),
        Err(e) => Err(e).context("checking the password"),
    }
}

pub fn read_max_size(con: &Connection) -> u64 {
    con.query_row("SELECT value FROM meta WHERE key='max_size'", [], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .and_then(|s| s.parse().ok())
    .unwrap_or(0)
}

/// Page size of the open container. Under SQLCipher `PRAGMA page_size`
/// answers with its own `cipher_page_size` row, as text rather than an
/// integer, so both are accepted.
pub fn page_size(con: &Connection) -> Result<u64> {
    con.query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0))
        .map(|v| v as u64)
        .or_else(|_| con.query_row("PRAGMA page_size", [], |r| r.get::<_, String>(0)).map(|s| s.parse().unwrap_or(4096)))
        .map_err(Into::into)
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
            "INSERT INTO inodes (kind, mode, uid, gid, size, atime, mtime, ctime) \
             VALUES (?1, 420, 0, 0, 0, 0.0, 0.0, 0.0)",
            rusqlite::params![KIND_FILE],
        )
        .unwrap();
        let ino = con.last_insert_rowid();
        con.execute("INSERT INTO dentries (parent, name, ino) VALUES (1, 'f', ?1)", rusqlite::params![ino])
            .unwrap();
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

    /// Builds a container in the version-1 layout by hand - the only way
    /// to test the migration now that `create_container` writes version 2.
    fn v1_container(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let path = dir.path().join("v1.coffer");
        let con = Connection::open(&path).unwrap();
        con.execute_batch(&pragma_key_sql("key", "test-password")).unwrap();
        con.execute_batch(
            "PRAGMA cipher_page_size = 4096;
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE inodes (
                 ino INTEGER PRIMARY KEY AUTOINCREMENT, parent INTEGER NOT NULL, name TEXT NOT NULL,
                 kind INTEGER NOT NULL, mode INTEGER NOT NULL, uid INTEGER NOT NULL, gid INTEGER NOT NULL,
                 size INTEGER NOT NULL DEFAULT 0, atime REAL NOT NULL, mtime REAL NOT NULL,
                 ctime REAL NOT NULL, symlink_target TEXT, UNIQUE(parent, name));
             CREATE INDEX idx_inodes_parent ON inodes(parent);
             CREATE TABLE data (ino INTEGER NOT NULL, block_no INTEGER NOT NULL, content BLOB NOT NULL,
                 PRIMARY KEY (ino, block_no)) WITHOUT ROWID;
             INSERT INTO meta (key, value) VALUES ('schema_version', '1'), ('block_size', '131072'), ('max_size', '0');
             INSERT INTO inodes (ino, parent, name, kind, mode, uid, gid, size, atime, mtime, ctime)
                 VALUES (1, 0, '', 0, 16832, 0, 0, 0, 0.0, 0.0, 0.0);
             INSERT INTO inodes (ino, parent, name, kind, mode, uid, gid, size, atime, mtime, ctime)
                 VALUES (2, 1, 'sub', 0, 16877, 0, 0, 0, 0.0, 0.0, 0.0),
                        (7, 1, 'a.txt', 1, 33188, 0, 0, 5, 0.0, 0.0, 0.0),
                        (9, 2, 'b.txt', 1, 33188, 0, 0, 3, 0.0, 0.0, 0.0);
             INSERT INTO data (ino, block_no, content) VALUES (7, 0, x'68656c6c6f');
             -- Created and deleted again, the way any container looks whose
             -- newest file has since been removed: the AUTOINCREMENT
             -- sequence stays at 12 while the highest surviving inode is 9.
             INSERT INTO inodes (ino, parent, name, kind, mode, uid, gid, size, atime, mtime, ctime)
                 VALUES (12, 1, 'gone.txt', 1, 33188, 0, 0, 0, 0.0, 0.0, 0.0);
             DELETE FROM inodes WHERE ino = 12;
             PRAGMA journal_mode = WAL;",
        )
        .unwrap();
        drop(con);
        path
    }

    #[test]
    fn inconsistencies_are_found_and_a_healthy_container_is_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.coffer");
        create_container(&path, "pw", 0).unwrap();
        let con = open_db(&path, "pw", false).unwrap();
        // A container straight out of create_container has nothing wrong.
        assert_eq!(find_inconsistencies(&con).unwrap(), (0, 0));

        // One file, properly linked: still nothing wrong.
        con.execute(
            "INSERT INTO inodes (ino, kind, mode, uid, gid, size, atime, mtime, ctime) \
             VALUES (2, 1, 420, 0, 0, 0, 0.0, 0.0, 0.0)",
            [],
        )
        .unwrap();
        con.execute("INSERT INTO dentries (parent, name, ino) VALUES (1, 'f', 2)", []).unwrap();
        assert_eq!(find_inconsistencies(&con).unwrap(), (0, 0));

        // A name for an inode that was never there.
        con.execute("INSERT INTO dentries (parent, name, ino) VALUES (1, 'ghost', 99)", []).unwrap();
        assert_eq!(find_inconsistencies(&con).unwrap(), (1, 0), "a dangling name must be counted");

        // An inode no name points at - its blocks would be unreachable.
        con.execute("DELETE FROM dentries WHERE name = 'ghost'", []).unwrap();
        con.execute("DELETE FROM dentries WHERE name = 'f'", []).unwrap();
        assert_eq!(find_inconsistencies(&con).unwrap(), (0, 1), "an unreachable inode must be counted");

        // The root has no name by design and must never be counted as one.
        con.execute("DELETE FROM inodes WHERE ino = 2", []).unwrap();
        assert_eq!(find_inconsistencies(&con).unwrap(), (0, 0));
    }

    #[test]
    fn a_read_only_open_leaves_version_1_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = v1_container(&dir);
        let con = open_db(&path, "test-password", true).unwrap();
        assert_eq!(schema_version(&con).as_deref(), Some("1"), "read-only must not migrate");
        // The commands that work on either version still answer.
        let files: i64 = con.query_row("SELECT count(*) FROM inodes WHERE kind=1", [], |r| r.get(0)).unwrap();
        assert_eq!(files, 2);
    }

    #[test]
    fn a_writable_open_migrates_version_1_and_keeps_everything() {
        let dir = tempfile::tempdir().unwrap();
        let path = v1_container(&dir);
        {
            let con = open_db(&path, "test-password", false).unwrap();
            assert_eq!(schema_version(&con).as_deref(), Some("2"));

            // Every name survived, pointing at the inode it did before.
            let mut names: Vec<(i64, String, i64)> = con
                .prepare("SELECT parent, name, ino FROM dentries ORDER BY ino")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            names.sort();
            assert_eq!(
                names,
                vec![(1, "a.txt".to_string(), 7), (1, "sub".to_string(), 2), (2, "b.txt".to_string(), 9)]
            );
            // The root keeps its inode and gains no entry of its own.
            let root: i64 = con.query_row("SELECT count(*) FROM dentries WHERE ino=1", [], |r| r.get(0)).unwrap();
            assert_eq!(root, 0);
            let inodes: i64 = con.query_row("SELECT count(*) FROM inodes", [], |r| r.get(0)).unwrap();
            assert_eq!(inodes, 4, "root plus three entries");
            // Content is untouched.
            let block: Vec<u8> = con
                .query_row("SELECT content FROM data WHERE ino=7 AND block_no=0", [], |r| r.get(0))
                .unwrap();
            assert_eq!(block, b"hello");
            // The xattrs table and its trigger are back after the rebuild.
            assert!(has_xattrs(&con));
            let trigger: i64 = con
                .query_row("SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name='xattrs_gc'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(trigger, 1);
            // Inode numbers carry on above every number ever handed out -
            // including 12, which no row carries any more because that file
            // was deleted before the upgrade. Copying the surviving rows
            // alone would leave the sequence at 9 and hand 12 out twice.
            con.execute(
                "INSERT INTO inodes (kind, mode, uid, gid, size, atime, mtime, ctime) VALUES (1, 420, 0, 0, 0, 0.0, 0.0, 0.0)",
                [],
            )
            .unwrap();
            assert_eq!(
                con.last_insert_rowid(),
                13,
                "AUTOINCREMENT must continue past the deleted inode 12, not reuse it"
            );
        }
        // Opening it again is a plain version-2 open, nothing left to do.
        let con = open_db(&path, "test-password", false).unwrap();
        assert_eq!(schema_version(&con).as_deref(), Some("2"));
    }

    /// What the NFS fix rests on: a mount connection never creates the
    /// mmap()ed -shm file (whose locks were the per-transaction round
    /// trips), and while it is open nothing else gets in - with a message
    /// that says "mounted", not "wrong password".
    #[test]
    fn mount_connection_is_exclusive_and_skips_shm() {
        let (dir, path) = temp_container();
        let shm = dir.path().join("t.coffer-shm");
        let con = open_db_for_mount(&path, "test-password", false, true).unwrap();
        con.execute("UPDATE meta SET value=value WHERE key='max_size'", []).unwrap();
        assert!(!shm.exists(), "exclusive WAL must keep its index in heap memory");
        for readonly in [true, false] {
            let err = open_db(&path, "test-password", readonly).unwrap_err().to_string();
            assert!(err.contains("is mounted"), "{err}");
        }
        drop(con);
        open_db(&path, "test-password", true).expect("free again once the mount connection closes");
    }

    /// Also the regression test for read_first_page: opening and closing the
    /// container file in the mount's process would drop the mount's locks,
    /// and the container would no longer be "mounted" to anyone else.
    #[test]
    fn verify_password_works_while_mounted_exclusively() {
        let (dir, path) = temp_container();
        let con = open_db_for_mount(&path, "test-password", false, true).unwrap();
        con.execute("UPDATE meta SET value=value WHERE key='max_size'", []).unwrap();
        let page = read_first_page(&con).unwrap();
        assert!(verify_password(&page, "test-password", dir.path()).unwrap());
        assert!(!verify_password(&page, "wrong", dir.path()).unwrap());
        let err = open_db(&path, "test-password", true).unwrap_err().to_string();
        assert!(err.contains("is mounted"), "the mount must still hold its locks: {err}");
    }

    #[test]
    fn locking_auto_follows_the_filesystem() {
        let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        assert!(!Locking::Normal.exclusive_for(dir.path()));
        assert!(Locking::Exclusive.exclusive_for(dir.path()));
        assert_eq!(Locking::Auto.exclusive_for(dir.path()), on_network_fs(dir.path()));
        assert_eq!(Locking::parse(" Exclusive "), Some(Locking::Exclusive));
        assert_eq!(Locking::parse("sometimes"), None);
    }

    /// A read-only mount can't be exclusive (see open_db_for_mount) and
    /// quietly keeps the normal mode, so another reader still gets in.
    #[test]
    fn read_only_mount_connection_works() {
        let (_dir, path) = temp_container();
        let con = open_db_for_mount(&path, "test-password", true, true).unwrap();
        assert_eq!(read_max_size(&con), 0);
        open_db(&path, "test-password", true).expect("not exclusive");
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
