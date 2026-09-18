use crate::db::{self, BLOCK_SIZE, KIND_DIR, KIND_FILE, KIND_SYMLINK, ROOT_INO};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo, InitFlags,
    KernelConfig, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    RenameFlags, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);

// The `size` column is SQLite INTEGER (signed 64-bit); a length beyond this
// would wrap to negative when persisted. Nowhere near any real disk or
// legitimate offset, but pwrite()/ftruncate() let a caller pass an
// arbitrary offset, so this is enforced explicitly rather than left to
// silently wrap (release builds don't panic on overflow - see Cargo.toml).
const MAX_FILE_SIZE: u64 = i64::MAX as u64;

// Linux's own limits for extended attributes (XATTR_NAME_MAX, XATTR_SIZE_MAX),
// so a value that would be refused on ext4 is refused here too, with the
// same errno, instead of quietly growing the container.
const XATTR_NAME_MAX: usize = 255;
const XATTR_SIZE_MAX: usize = 65536;

struct InodeRow {
    kind: i64,
    mode: i64,
    uid: u32,
    gid: u32,
    size: u64,
    atime: f64,
    mtime: f64,
    ctime: f64,
    symlink_target: Option<String>,
}

pub struct CofferFS {
    con: Arc<Mutex<Connection>>,
    max_size: u64,
    uid: u32,
    gid: u32,
    container_dir: PathBuf,
    last_activity: Arc<AtomicU64>,
    read_only: bool,
    /// False only for a read-only mount of a container that predates the
    /// xattrs table (see db::has_xattrs); every xattr call then answers as
    /// if the file had none.
    has_xattrs: bool,
}

impl CofferFS {
    pub fn new(con: Connection, max_size: u64, container_path: &Path, read_only: bool) -> Self {
        let has_xattrs = db::has_xattrs(&con);
        CofferFS {
            con: Arc::new(Mutex::new(con)),
            max_size,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            container_dir: container_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .to_path_buf(),
            last_activity: Arc::new(AtomicU64::new(db::now_secs() as u64)),
            read_only,
            has_xattrs,
        }
    }

    /// Shared handle an idle-timeout watcher can poll from outside the FUSE
    /// loop; every handled call below refreshes it via `touch()`.
    pub fn last_activity(&self) -> Arc<AtomicU64> {
        self.last_activity.clone()
    }

    /// Shared handle for an out-of-band idle-compact watcher to run VACUUM
    /// on - the same connection/lock the FUSE loop itself uses, so it's
    /// automatically serialized with any live filesystem activity.
    pub fn connection_handle(&self) -> Arc<Mutex<Connection>> {
        self.con.clone()
    }

    fn touch(&self) {
        self.last_activity.store(db::now_secs() as u64, Ordering::Relaxed);
    }
}

// --- small helpers over the schema, shared by every FUSE callback ----------

fn map_row(r: &rusqlite::Row) -> rusqlite::Result<InodeRow> {
    Ok(InodeRow {
        kind: r.get(1)?,
        mode: r.get(2)?,
        uid: r.get::<_, i64>(3)? as u32,
        gid: r.get::<_, i64>(4)? as u32,
        size: r.get::<_, i64>(5)? as u64,
        atime: r.get(6)?,
        mtime: r.get(7)?,
        ctime: r.get(8)?,
        symlink_target: r.get(9)?,
    })
}

const SELECT_ROW_BY_INO: &str = "SELECT ino, kind, mode, uid, gid, size, atime, mtime, ctime, symlink_target FROM inodes WHERE ino=?1";

fn row_by_ino(con: &Connection, ino: u64) -> rusqlite::Result<Option<InodeRow>> {
    con.prepare_cached(SELECT_ROW_BY_INO)?
        .query_row(params![ino as i64], map_row)
        .optional()
}

fn child_ino(con: &Connection, parent: u64, name: &str) -> rusqlite::Result<Option<u64>> {
    con.prepare_cached("SELECT ino FROM inodes WHERE parent=?1 AND name=?2")?
        .query_row(params![parent as i64, name], |r| r.get::<_, i64>(0))
        .optional()
        .map(|o| o.map(|v| v as u64))
}

fn has_children(con: &Connection, ino: u64) -> rusqlite::Result<bool> {
    con.prepare_cached("SELECT 1 FROM inodes WHERE parent=?1 LIMIT 1")?
        .query_row(params![ino as i64], |_| Ok(()))
        .optional()
        .map(|o| o.is_some())
}

fn get_block(con: &Connection, ino: u64, block_no: i64) -> rusqlite::Result<Vec<u8>> {
    con.prepare_cached("SELECT content FROM data WHERE ino=?1 AND block_no=?2")?
        .query_row(params![ino as i64, block_no], |r| r.get::<_, Vec<u8>>(0))
        .optional()
        .map(|o| o.unwrap_or_default())
}

fn put_block(con: &Connection, ino: u64, block_no: i64, content: &[u8]) -> rusqlite::Result<()> {
    if content.is_empty() {
        con.prepare_cached("DELETE FROM data WHERE ino=?1 AND block_no=?2")?
            .execute(params![ino as i64, block_no])?;
    } else {
        con.prepare_cached(
            "INSERT INTO data (ino, block_no, content) VALUES (?1, ?2, ?3) \
             ON CONFLICT(ino, block_no) DO UPDATE SET content=excluded.content",
        )?
        .execute(params![ino as i64, block_no, content])?;
    }
    Ok(())
}

fn current_total_size(con: &Connection) -> rusqlite::Result<u64> {
    con.prepare_cached("SELECT COALESCE(SUM(size), 0) FROM inodes")?
        .query_row([], |r| r.get::<_, i64>(0))
        .map(|v| v as u64)
}

#[allow(clippy::too_many_arguments)]
fn insert_child(
    con: &Connection,
    parent: u64,
    name: &str,
    kind: i64,
    mode: i64,
    uid: u32,
    gid: u32,
    symlink_target: Option<&str>,
) -> rusqlite::Result<u64> {
    let now = db::now_secs();
    let size = symlink_target.map(|s| s.len() as i64).unwrap_or(0);
    con.prepare_cached(
        "INSERT INTO inodes (parent, name, kind, mode, uid, gid, size, atime, mtime, ctime, symlink_target) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?8, ?9)",
    )?
    .execute(params![parent as i64, name, kind, mode, uid, gid, size, now, symlink_target])?;
    let new_ino = con.last_insert_rowid() as u64;
    con.prepare_cached("UPDATE inodes SET mtime=?1, ctime=?1 WHERE ino=?2")?
        .execute(params![now, parent as i64])?;
    Ok(new_ino)
}

/// A directory may only be removed, or replaced by a rename, while it is
/// empty. The kernel rules out directory-over-file and file-over-directory
/// itself but leaves this one to the filesystem, and getting it wrong
/// orphans the whole subtree: rows still there, reachable by nothing.
/// Shared by rmdir and rename so the answer can only be decided once.
fn ensure_removable(con: &Connection, ino: u64) -> Result<(), Errno> {
    let kind: rusqlite::Result<i64> = con
        .prepare_cached("SELECT kind FROM inodes WHERE ino=?1")
        .and_then(|mut s| s.query_row(params![ino as i64], |r| r.get(0)));
    match kind {
        Ok(KIND_DIR) => match has_children(con, ino) {
            Ok(true) => Err(Errno::ENOTEMPTY),
            Ok(false) => Ok(()),
            Err(_) => Err(Errno::EIO),
        },
        Ok(_) => Ok(()),
        Err(_) => Err(Errno::EIO),
    }
}

/// Drop what a modification has to drop from a file: the setuid bit, the
/// setgid bit *if the file is group-executable* (without that bit S_ISGID
/// marks mandatory locking, not a privilege, and Linux leaves it alone),
/// and the `security.capability` attribute. Because coffer negotiates
/// FUSE_HANDLE_KILLPRIV_V2 (see init()) the kernel stops doing this and
/// hands the duty here - for writes, truncates and chowns alike.
///
/// SQLite has no octal literals: 2048 is S_ISUID (04000), 1024 S_ISGID
/// (02000), 8 S_IXGRP (010). Each statement is a no-op unless there is
/// something to drop, so the common case costs three primary-key probes.
fn kill_priv(con: &Connection, ino: u64) -> rusqlite::Result<()> {
    con.prepare_cached("UPDATE inodes SET mode = mode & ~2048 WHERE ino=?1 AND (mode & 2048) != 0")?
        .execute(params![ino as i64])?;
    con.prepare_cached(
        "UPDATE inodes SET mode = mode & ~1024 WHERE ino=?1 AND (mode & 1024) != 0 AND (mode & 8) != 0",
    )?
    .execute(params![ino as i64])?;
    con.prepare_cached("DELETE FROM xattrs WHERE ino=?1 AND name='security.capability'")?
        .execute(params![ino as i64])?;
    Ok(())
}

/// Everything an inode owns, gone in one place: its blocks, its extended
/// attributes (also covered by the xattrs_gc trigger, kept explicit so the
/// intent is visible here) and the inode row itself. unlink, rmdir and a
/// rename over an existing entry all end up here.
fn delete_inode(con: &Connection, ino: u64) -> rusqlite::Result<()> {
    con.prepare_cached("DELETE FROM data WHERE ino=?1")?.execute(params![ino as i64])?;
    con.prepare_cached("DELETE FROM xattrs WHERE ino=?1")?.execute(params![ino as i64])?;
    con.prepare_cached("DELETE FROM inodes WHERE ino=?1")?.execute(params![ino as i64])?;
    Ok(())
}

fn truncate_inode(con: &Connection, ino: u64, length: u64) -> rusqlite::Result<()> {
    let last_block = (length / BLOCK_SIZE as u64) as i64;
    let last_off = (length % BLOCK_SIZE as u64) as usize;
    con.prepare_cached("DELETE FROM data WHERE ino=?1 AND block_no>?2")?
        .execute(params![ino as i64, last_block])?;
    if last_off > 0 {
        let mut block = get_block(con, ino, last_block)?;
        block.truncate(last_off);
        put_block(con, ino, last_block, &block)?;
    } else {
        con.prepare_cached("DELETE FROM data WHERE ino=?1 AND block_no=?2")?
            .execute(params![ino as i64, last_block])?;
    }
    let now = db::now_secs();
    con.prepare_cached("UPDATE inodes SET size=?1, mtime=?2, ctime=?2 WHERE ino=?3")?
        .execute(params![length as i64, now, ino as i64])?;
    Ok(())
}

fn secs_to_systemtime(s: f64) -> SystemTime {
    if s <= 0.0 {
        UNIX_EPOCH
    } else {
        UNIX_EPOCH + Duration::from_secs_f64(s)
    }
}

fn time_or_now_to_secs(t: Option<TimeOrNow>) -> Option<f64> {
    t.map(|t| match t {
        TimeOrNow::SpecificTime(st) => st
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
        TimeOrNow::Now => db::now_secs(),
    })
}

fn file_type_of(kind: i64) -> FileType {
    match kind {
        KIND_DIR => FileType::Directory,
        KIND_SYMLINK => FileType::Symlink,
        _ => FileType::RegularFile,
    }
}

fn to_attr(ino: u64, row: &InodeRow) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size: row.size,
        blocks: row.size.div_ceil(512),
        atime: secs_to_systemtime(row.atime),
        mtime: secs_to_systemtime(row.mtime),
        ctime: secs_to_systemtime(row.ctime),
        crtime: secs_to_systemtime(row.ctime),
        kind: file_type_of(row.kind),
        perm: (row.mode & 0o7777) as u16,
        nlink: if row.kind == KIND_DIR { 2 } else { 1 },
        uid: row.uid,
        gid: row.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// The size protocol shared by getxattr and listxattr: with size 0 the
/// caller only wants to know how big the answer is; otherwise the answer
/// must fit or it's ERANGE.
fn reply_xattr(reply: ReplyXattr, size: u32, data: &[u8]) {
    if size == 0 {
        reply.size(data.len() as u32);
    } else if data.len() > size as usize {
        reply.error(Errno::ERANGE);
    } else {
        reply.data(data);
    }
}

// Every DB error elsewhere in this file collapses to EIO, which is fine for
// operations that only ever touch a row or two of metadata. write()/create()
// are the paths that can plausibly move enough data to hit a genuinely full
// host disk (relevant when --max-size is unset, i.e. "grows until host disk
// is full") - callers/tools checking for ENOSPC specifically (cp, GUI file
// managers) deserve the real errno there instead of a generic I/O error.
fn errno_for(e: &rusqlite::Error) -> Errno {
    if let rusqlite::Error::SqliteFailure(inner, _) = e {
        if inner.code == rusqlite::ErrorCode::DiskFull {
            return Errno::ENOSPC;
        }
    }
    Errno::EIO
}

// --- the actual FUSE filesystem --------------------------------------------

impl Filesystem for CofferFS {
    // Once a filesystem answers getxattr at all (ENODATA rather than
    // ENOSYS), the kernel asks it for `security.capability` before every
    // buffered write to decide whether file capabilities must be dropped -
    // one extra round trip per write(2), serialised on the single FUSE
    // thread. FUSE_HANDLE_KILLPRIV_V2 (kernel 5.11+) moves that duty here:
    // the kernel stops asking and instead flags writes that must clear
    // setuid/setgid and `security.capability` (see write()). Older kernels
    // reject the capability, which just leaves the round trip in place.
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        let _ = config.add_capabilities(InitFlags::FUSE_HANDLE_KILLPRIV_V2);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.touch();
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let con = self.con.lock().unwrap();
        match child_ino(&con, parent.0, name) {
            Ok(Some(ino)) => match row_by_ino(&con, ino) {
                Ok(Some(row)) => reply.entry(&TTL, &to_attr(ino, &row), Generation(0)),
                _ => reply.error(Errno::EIO),
            },
            Ok(None) => reply.error(Errno::ENOENT),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.touch();
        let con = self.con.lock().unwrap();
        match row_by_ino(&con, ino.0) {
            Ok(Some(row)) => reply.attr(&TTL, &to_attr(ino.0, &row)),
            Ok(None) => reply.error(Errno::ENOENT),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        self.touch();
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        let mut con = self.con.lock().unwrap();
        let tx = match con.transaction() {
            Ok(t) => t,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let row = match row_by_ino(&tx, ino.0) {
            Ok(Some(r)) => r,
            Ok(None) => {
                reply.error(Errno::ENOENT);
                return;
            }
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let now = db::now_secs();
        let mut ok = true;

        if let Some(m) = mode {
            let kind_bits = row.mode & !0o7777;
            ok &= tx
                .prepare_cached("UPDATE inodes SET mode=?1, ctime=?2 WHERE ino=?3")
                .and_then(|mut s| s.execute(params![kind_bits | (m as i64 & 0o7777), now, ino.0 as i64]))
                .is_ok();
        }
        if uid.is_some() || gid.is_some() {
            let new_uid = uid.unwrap_or(row.uid);
            let new_gid = gid.unwrap_or(row.gid);
            ok &= tx
                .prepare_cached("UPDATE inodes SET uid=?1, gid=?2, ctime=?3 WHERE ino=?4")
                .and_then(|mut s| s.execute(params![new_uid, new_gid, now, ino.0 as i64]))
                .is_ok();
        }
        if let Some(new_len) = size {
            if new_len > MAX_FILE_SIZE {
                reply.error(Errno::EFBIG);
                return;
            }
            // Growing via truncate/ftruncate (e.g. pre-allocating a sparse
            // file) needs the same ceiling check write() does - otherwise
            // `--max-size` is trivially bypassed with a single ftruncate to
            // a huge length, no actual data required.
            if self.max_size != 0 && new_len > row.size {
                if let Ok(used) = current_total_size(&tx) {
                    if used + (new_len - row.size) > self.max_size {
                        reply.error(Errno::ENOSPC);
                        return;
                    }
                }
            }
            ok &= truncate_inode(&tx, ino.0, new_len).is_ok();
        }
        if atime.is_some() || mtime.is_some() {
            let a = time_or_now_to_secs(atime).unwrap_or(row.atime);
            let m = time_or_now_to_secs(mtime).unwrap_or(row.mtime);
            ok &= tx
                .prepare_cached("UPDATE inodes SET atime=?1, mtime=?2 WHERE ino=?3")
                .and_then(|mut s| s.execute(params![a, m, ino.0 as i64]))
                .is_ok();
        }

        // The other half of FUSE_HANDLE_KILLPRIV_V2 (see init()): the
        // capability makes the filesystem responsible for dropping
        // setuid/setgid and file capabilities on write, truncate *and*
        // chown. For write the kernel flags the request; for these two it
        // does not, and fuser 0.18 surfaces neither FATTR_KILL_SUIDGID nor
        // FUSE_OPEN_KILL_SUIDGID, so the rule is applied here, matching
        // what the VFS would have done:
        //   - truncate: by a caller without CAP_FSETID (uid 0 stands in for
        //     the capability, as coffer has no way to ask for the real one),
        //   - chown of a non-directory: always, privileged or not,
        //   - never when the same call sets the mode explicitly: an
        //     intentional `chmod u+s` must not be undone by its own request.
        let truncating = size.is_some() && req.uid() != 0;
        let chowning = (uid.is_some() || gid.is_some()) && row.kind != KIND_DIR;
        if mode.is_none() && (truncating || chowning) {
            ok &= kill_priv(&tx, ino.0).is_ok();
        }

        if !ok || tx.commit().is_err() {
            reply.error(Errno::EIO);
            return;
        }
        match row_by_ino(&con, ino.0) {
            Ok(Some(row)) => reply.attr(&TTL, &to_attr(ino.0, &row)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        self.touch();
        let con = self.con.lock().unwrap();
        match row_by_ino(&con, ino.0) {
            Ok(Some(row)) => reply.data(row.symlink_target.unwrap_or_default().as_bytes()),
            Ok(None) => reply.error(Errno::ENOENT),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        self.touch();
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let mut con = self.con.lock().unwrap();
        let tx = match con.transaction() {
            Ok(t) => t,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        match child_ino(&tx, parent.0, name) {
            Ok(Some(_)) => {
                reply.error(Errno::EEXIST);
                return;
            }
            Ok(None) => {}
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        }
        let file_mode = (libc::S_IFDIR | (mode & 0o7777)) as i64;
        let new_ino = match insert_child(&tx, parent.0, name, KIND_DIR, file_mode, self.uid, self.gid, None) {
            Ok(i) => i,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        if tx.commit().is_err() {
            reply.error(Errno::EIO);
            return;
        }
        match row_by_ino(&con, new_ino) {
            Ok(Some(row)) => reply.entry(&TTL, &to_attr(new_ino, &row), Generation(0)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.touch();
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let mut con = self.con.lock().unwrap();
        let tx = match con.transaction() {
            Ok(t) => t,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let ino = match child_ino(&tx, parent.0, name) {
            Ok(Some(i)) => i,
            Ok(None) => {
                reply.error(Errno::ENOENT);
                return;
            }
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let now = db::now_secs();
        let ok = delete_inode(&tx, ino).is_ok()
            && tx
                .prepare_cached("UPDATE inodes SET mtime=?1, ctime=?1 WHERE ino=?2")
                .and_then(|mut s| s.execute(params![now, parent.0 as i64]))
                .is_ok();
        if !ok || tx.commit().is_err() {
            reply.error(Errno::EIO);
            return;
        }
        reply.ok();
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.touch();
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let mut con = self.con.lock().unwrap();
        let tx = match con.transaction() {
            Ok(t) => t,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let ino = match child_ino(&tx, parent.0, name) {
            Ok(Some(i)) => i,
            Ok(None) => {
                reply.error(Errno::ENOENT);
                return;
            }
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        if ino == ROOT_INO {
            reply.error(Errno::EBUSY);
            return;
        }
        if let Err(e) = ensure_removable(&tx, ino) {
            reply.error(e);
            return;
        }
        let now = db::now_secs();
        let ok = delete_inode(&tx, ino).is_ok()
            && tx
                .prepare_cached("UPDATE inodes SET mtime=?1, ctime=?1 WHERE ino=?2")
                .and_then(|mut s| s.execute(params![now, parent.0 as i64]))
                .is_ok();
        if !ok || tx.commit().is_err() {
            reply.error(Errno::EIO);
            return;
        }
        reply.ok();
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        self.touch();
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        let (Some(name), Some(target_str)) = (link_name.to_str(), target.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let mut con = self.con.lock().unwrap();
        let tx = match con.transaction() {
            Ok(t) => t,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        if let Ok(Some(_)) = child_ino(&tx, parent.0, name) {
            reply.error(Errno::EEXIST);
            return;
        }
        if self.max_size != 0 {
            if let Ok(used) = current_total_size(&tx) {
                if used + target_str.len() as u64 > self.max_size {
                    reply.error(Errno::ENOSPC);
                    return;
                }
            }
        }
        let mode = (libc::S_IFLNK | 0o777) as i64;
        let new_ino = match insert_child(&tx, parent.0, name, KIND_SYMLINK, mode, self.uid, self.gid, Some(target_str)) {
            Ok(i) => i,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        if tx.commit().is_err() {
            reply.error(Errno::EIO);
            return;
        }
        match row_by_ino(&con, new_ino) {
            Ok(Some(row)) => reply.entry(&TTL, &to_attr(new_ino, &row), Generation(0)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        self.touch();
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let mut con = self.con.lock().unwrap();
        let tx = match con.transaction() {
            Ok(t) => t,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let ino = match child_ino(&tx, parent.0, name) {
            Ok(Some(i)) => i,
            Ok(None) => {
                reply.error(Errno::ENOENT);
                return;
            }
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        // Renaming over an existing entry replaces it - unless the caller
        // used renameat2(2) to ask for something else. RENAME_NOREPLACE
        // ("don't overwrite", what `mv -n` uses) must fail with EEXIST
        // instead of deleting the target; RENAME_EXCHANGE (an atomic swap)
        // and RENAME_WHITEOUT are not implemented, and saying so with
        // EINVAL is what a filesystem does for a rename flag it does not
        // support. Silently ignoring either destroys the target.
        let target = match child_ino(&tx, newparent.0, newname) {
            Ok(t) => t,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        if flags.intersects(RenameFlags::RENAME_EXCHANGE | RenameFlags::RENAME_WHITEOUT) {
            reply.error(Errno::EINVAL);
            return;
        }
        if flags.contains(RenameFlags::RENAME_NOREPLACE) && target.is_some() {
            reply.error(Errno::EEXIST);
            return;
        }
        if let Some(existing) = target {
            if existing != ino {
                // The kernel already rules out file-over-directory (EISDIR)
                // and directory-over-file (ENOTDIR); a directory may only be
                // replaced while empty.
                if let Err(e) = ensure_removable(&tx, existing) {
                    reply.error(e);
                    return;
                }
                if delete_inode(&tx, existing).is_err() {
                    reply.error(Errno::EIO);
                    return;
                }
            }
        }
        let now = db::now_secs();
        let ok = tx
            .prepare_cached("UPDATE inodes SET parent=?1, name=?2 WHERE ino=?3")
            .and_then(|mut s| s.execute(params![newparent.0 as i64, newname, ino as i64]))
            .is_ok()
            && tx
                .prepare_cached("UPDATE inodes SET mtime=?1, ctime=?1 WHERE ino IN (?2, ?3)")
                .and_then(|mut s| s.execute(params![now, parent.0 as i64, newparent.0 as i64]))
                .is_ok();
        if !ok || tx.commit().is_err() {
            reply.error(Errno::EIO);
            return;
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: fuser::OpenFlags, reply: ReplyOpen) {
        self.touch();
        // The kernel already refuses writes on an `ro` mount; this is the
        // belt to that suspenders, for the case the mount option ever gets
        // lost (a remount, a foreign mount helper).
        if self.read_only && (flags.0 & libc::O_ACCMODE) != libc::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }
        reply.opened(FileHandle(ino.0), fuser::FopenFlags::empty());
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        self.touch();
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let mut con = self.con.lock().unwrap();
        let tx = match con.transaction() {
            Ok(t) => t,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        match child_ino(&tx, parent.0, name) {
            Ok(Some(_)) => {
                reply.error(Errno::EEXIST);
                return;
            }
            Ok(None) => {}
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        }
        if self.max_size != 0 {
            if let Ok(used) = current_total_size(&tx) {
                if used >= self.max_size {
                    reply.error(Errno::ENOSPC);
                    return;
                }
            }
        }
        let file_mode = (libc::S_IFREG | (mode & 0o7777)) as i64;
        let new_ino = match insert_child(&tx, parent.0, name, KIND_FILE, file_mode, self.uid, self.gid, None) {
            Ok(i) => i,
            Err(e) => {
                reply.error(errno_for(&e));
                return;
            }
        };
        if let Err(e) = tx.commit() {
            reply.error(errno_for(&e));
            return;
        }
        match row_by_ino(&con, new_ino) {
            Ok(Some(row)) => reply.created(
                &TTL,
                &to_attr(new_ino, &row),
                Generation(0),
                FileHandle(new_ino),
                fuser::FopenFlags::empty(),
            ),
            _ => reply.error(Errno::EIO),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        self.touch();
        let ino = fh.0;
        let con = self.con.lock().unwrap();
        let file_size = match row_by_ino(&con, ino) {
            Ok(Some(row)) => row.size,
            Ok(None) => {
                reply.error(Errno::ENOENT);
                return;
            }
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let end = std::cmp::min(offset + size as u64, file_size);
        if end <= offset {
            reply.data(&[]);
            return;
        }
        let mut out = Vec::with_capacity((end - offset) as usize);
        let mut pos = offset;
        while pos < end {
            let block_no = (pos / BLOCK_SIZE as u64) as i64;
            let block_off = (pos % BLOCK_SIZE as u64) as usize;
            let chunk_len = std::cmp::min(BLOCK_SIZE as u64 - block_off as u64, end - pos) as usize;
            let block = get_block(&con, ino, block_no).unwrap_or_default();
            if block_off < block.len() {
                let avail = std::cmp::min(chunk_len, block.len() - block_off);
                out.extend_from_slice(&block[block_off..block_off + avail]);
                if avail < chunk_len {
                    out.resize(out.len() + (chunk_len - avail), 0);
                }
            } else {
                out.resize(out.len() + chunk_len, 0);
            }
            pos += chunk_len as u64;
        }
        reply.data(&out);
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        write_flags: WriteFlags,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        self.touch();
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        if offset > MAX_FILE_SIZE || data.len() as u64 > MAX_FILE_SIZE - offset {
            reply.error(Errno::EFBIG);
            return;
        }
        let ino = fh.0;
        let mut con = self.con.lock().unwrap();
        let tx = match con.transaction() {
            Ok(t) => t,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let old_size = match row_by_ino(&tx, ino) {
            Ok(Some(row)) => row.size,
            Ok(None) => {
                reply.error(Errno::ENOENT);
                return;
            }
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let new_size_needed = offset + data.len() as u64;
        if self.max_size != 0 && new_size_needed > old_size {
            if let Ok(used) = current_total_size(&tx) {
                if used + (new_size_needed - old_size) > self.max_size {
                    reply.error(Errno::ENOSPC);
                    return;
                }
            }
        }
        let mut pos = offset;
        let mut remaining = data;
        while !remaining.is_empty() {
            let block_no = (pos / BLOCK_SIZE as u64) as i64;
            let block_off = (pos % BLOCK_SIZE as u64) as usize;
            let take = std::cmp::min(BLOCK_SIZE as usize - block_off, remaining.len());
            let chunk = &remaining[..take];
            let mut buf = match get_block(&tx, ino, block_no) {
                Ok(b) => b,
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            };
            let need_len = block_off + chunk.len();
            if buf.len() < need_len {
                buf.resize(need_len, 0);
            }
            buf[block_off..block_off + chunk.len()].copy_from_slice(chunk);
            if let Err(e) = put_block(&tx, ino, block_no, &buf) {
                reply.error(errno_for(&e));
                return;
            }
            pos += take as u64;
            remaining = &remaining[take..];
        }
        let new_size = std::cmp::max(old_size, offset + data.len() as u64);
        let now = db::now_secs();
        let update_result = tx
            .prepare_cached("UPDATE inodes SET size=?1, mtime=?2, ctime=?2 WHERE ino=?3")
            .and_then(|mut s| s.execute(params![new_size as i64, now, ino as i64]));
        if let Err(e) = update_result {
            reply.error(errno_for(&e));
            return;
        }
        // Our side of FUSE_HANDLE_KILLPRIV_V2 (see init()). Here the kernel
        // says when: it sets the flag for a writer without CAP_FSETID.
        if write_flags.contains(WriteFlags::FUSE_WRITE_KILL_SUIDGID) {
            if let Err(e) = kill_priv(&tx, ino) {
                reply.error(errno_for(&e));
                return;
            }
        }
        if let Err(e) = tx.commit() {
            reply.error(errno_for(&e));
            return;
        }
        reply.written(data.len() as u32);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        self.touch();
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        self.touch();
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.touch();
        reply.ok();
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        self.touch();
        reply.opened(FileHandle(0), fuser::FopenFlags::empty());
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        self.touch();
        let con = self.con.lock().unwrap();
        let parent_ino: u64 = if ino.0 == ROOT_INO {
            ROOT_INO
        } else {
            match con
                .prepare_cached("SELECT parent FROM inodes WHERE ino=?1")
                .and_then(|mut s| s.query_row(params![ino.0 as i64], |r| r.get::<_, i64>(0)))
            {
                Ok(p) => p as u64,
                Err(_) => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino.0, FileType::Directory, ".".to_string()),
            (parent_ino, FileType::Directory, "..".to_string()),
        ];
        let mut stmt = match con.prepare_cached("SELECT ino, name, kind FROM inodes WHERE parent=?1 ORDER BY ino") {
            Ok(s) => s,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let rows = stmt.query_map(params![ino.0 as i64], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        });
        match rows {
            Ok(iter) => {
                for row in iter.flatten() {
                    entries.push((row.0, file_type_of(row.2), row.1));
                }
            }
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        }

        for (i, (cino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            let next_offset = (i + 1) as u64;
            if reply.add(INodeNo(*cino), next_offset, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        self.touch();
        let con = self.con.lock().unwrap();
        let used = current_total_size(&con).unwrap_or(0);
        let (blocks, bfree, bavail) = if self.max_size > 0 {
            let total = self.max_size / 4096;
            let free = self.max_size.saturating_sub(used) / 4096;
            (total, free, free)
        } else {
            match nix::sys::statvfs::statvfs(&self.container_dir) {
                Ok(vfs) => (
                    vfs.blocks() as u64,
                    vfs.blocks_free() as u64,
                    vfs.blocks_available() as u64,
                ),
                Err(_) => (0, 0, 0),
            }
        };
        reply.statfs(blocks, bfree, bavail, 0, 1_000_000, 4096, 255, 4096);
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        self.touch();
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        if !self.has_xattrs {
            reply.error(Errno::ENOTSUP);
            return;
        }
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        if name.is_empty() || name.len() > XATTR_NAME_MAX {
            reply.error(Errno::ERANGE);
            return;
        }
        if value.len() > XATTR_SIZE_MAX {
            reply.error(Errno::E2BIG);
            return;
        }
        let mut con = self.con.lock().unwrap();
        let tx = match con.transaction() {
            Ok(t) => t,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        match row_by_ino(&tx, ino.0) {
            Ok(Some(_)) => {}
            Ok(None) => {
                reply.error(Errno::ENOENT);
                return;
            }
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        }
        // XATTR_CREATE / XATTR_REPLACE are setxattr(2)'s own semantics:
        // the caller asked for "only if absent" or "only if present".
        let exists = tx
            .prepare_cached("SELECT 1 FROM xattrs WHERE ino=?1 AND name=?2")
            .and_then(|mut s| s.exists(params![ino.0 as i64, name]));
        match exists {
            Ok(true) if flags & libc::XATTR_CREATE != 0 => {
                reply.error(Errno::EEXIST);
                return;
            }
            Ok(false) if flags & libc::XATTR_REPLACE != 0 => {
                reply.error(Errno::ENODATA);
                return;
            }
            Ok(_) => {}
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        }
        let now = db::now_secs();
        let ok = tx
            .prepare_cached("INSERT OR REPLACE INTO xattrs (ino, name, value) VALUES (?1, ?2, ?3)")
            .and_then(|mut s| s.execute(params![ino.0 as i64, name, value]))
            .is_ok()
            && tx
                .prepare_cached("UPDATE inodes SET ctime=?1 WHERE ino=?2")
                .and_then(|mut s| s.execute(params![now, ino.0 as i64]))
                .is_ok();
        if !ok || tx.commit().is_err() {
            reply.error(Errno::EIO);
            return;
        }
        reply.ok();
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        self.touch();
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        if !self.has_xattrs {
            reply.error(Errno::ENODATA);
            return;
        }
        let con = self.con.lock().unwrap();
        let value: rusqlite::Result<Option<Vec<u8>>> = con
            .prepare_cached("SELECT value FROM xattrs WHERE ino=?1 AND name=?2")
            .and_then(|mut s| s.query_row(params![ino.0 as i64, name], |r| r.get(0)).optional());
        match value {
            Ok(Some(v)) => reply_xattr(reply, size, &v),
            Ok(None) => reply.error(Errno::ENODATA),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        self.touch();
        if !self.has_xattrs {
            reply_xattr(reply, size, &[]);
            return;
        }
        let con = self.con.lock().unwrap();
        // The listxattr(2) format: every name NUL-terminated, concatenated.
        let names: rusqlite::Result<Vec<u8>> = con
            .prepare_cached("SELECT name FROM xattrs WHERE ino=?1 ORDER BY name")
            .and_then(|mut s| {
                let rows = s.query_map(params![ino.0 as i64], |r| r.get::<_, String>(0))?;
                let mut out = Vec::new();
                for name in rows {
                    out.extend_from_slice(name?.as_bytes());
                    out.push(0);
                }
                Ok(out)
            });
        match names {
            Ok(list) => reply_xattr(reply, size, &list),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.touch();
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        if !self.has_xattrs {
            reply.error(Errno::ENODATA);
            return;
        }
        let mut con = self.con.lock().unwrap();
        let tx = match con.transaction() {
            Ok(t) => t,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let removed = tx
            .prepare_cached("DELETE FROM xattrs WHERE ino=?1 AND name=?2")
            .and_then(|mut s| s.execute(params![ino.0 as i64, name]));
        match removed {
            Ok(0) => {
                reply.error(Errno::ENODATA);
                return;
            }
            Ok(_) => {}
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        }
        let now = db::now_secs();
        let ok = tx
            .prepare_cached("UPDATE inodes SET ctime=?1 WHERE ino=?2")
            .and_then(|mut s| s.execute(params![now, ino.0 as i64]))
            .is_ok();
        if !ok || tx.commit().is_err() {
            reply.error(Errno::EIO);
            return;
        }
        reply.ok();
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        self.touch();
        reply.ok(); // single-user container: whoever mounted it gets full access
    }
}
