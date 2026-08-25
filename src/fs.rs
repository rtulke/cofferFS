use crate::db::{self, BLOCK_SIZE, KIND_DIR, KIND_FILE, KIND_SYMLINK, ROOT_INO};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, TimeOrNow,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);

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
    con: Mutex<Connection>,
    max_size: u64,
    uid: u32,
    gid: u32,
    container_dir: PathBuf,
    last_activity: Arc<AtomicU64>,
}

impl CofferFS {
    pub fn new(con: Connection, max_size: u64, container_path: &Path) -> Self {
        CofferFS {
            con: Mutex::new(con),
            max_size,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            container_dir: container_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .to_path_buf(),
            last_activity: Arc::new(AtomicU64::new(db::now_secs() as u64)),
        }
    }

    /// Shared handle an idle-timeout watcher can poll from outside the FUSE
    /// loop; every handled call below refreshes it via `touch()`.
    pub fn last_activity(&self) -> Arc<AtomicU64> {
        self.last_activity.clone()
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

// --- the actual FUSE filesystem --------------------------------------------

impl Filesystem for CofferFS {
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
        _req: &Request,
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
        let ok = tx
            .prepare_cached("DELETE FROM data WHERE ino=?1")
            .and_then(|mut s| s.execute(params![ino as i64]))
            .is_ok()
            && tx
                .prepare_cached("DELETE FROM inodes WHERE ino=?1")
                .and_then(|mut s| s.execute(params![ino as i64]))
                .is_ok()
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
        match has_children(&tx, ino) {
            Ok(true) => {
                reply.error(Errno::ENOTEMPTY);
                return;
            }
            Ok(false) => {}
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        }
        let now = db::now_secs();
        let ok = tx
            .prepare_cached("DELETE FROM inodes WHERE ino=?1")
            .and_then(|mut s| s.execute(params![ino as i64]))
            .is_ok()
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
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        self.touch();
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
        if let Ok(Some(existing)) = child_ino(&tx, newparent.0, newname) {
            if existing != ino {
                let _ = tx
                    .prepare_cached("DELETE FROM data WHERE ino=?1")
                    .and_then(|mut s| s.execute(params![existing as i64]));
                let _ = tx
                    .prepare_cached("DELETE FROM inodes WHERE ino=?1")
                    .and_then(|mut s| s.execute(params![existing as i64]));
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

    fn open(&self, _req: &Request, ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        self.touch();
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
        _write_flags: fuser::WriteFlags,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        self.touch();
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
            if put_block(&tx, ino, block_no, &buf).is_err() {
                reply.error(Errno::EIO);
                return;
            }
            pos += take as u64;
            remaining = &remaining[take..];
        }
        let new_size = std::cmp::max(old_size, offset + data.len() as u64);
        let now = db::now_secs();
        let ok = tx
            .prepare_cached("UPDATE inodes SET size=?1, mtime=?2, ctime=?2 WHERE ino=?3")
            .and_then(|mut s| s.execute(params![new_size as i64, now, ino as i64]))
            .is_ok();
        if !ok || tx.commit().is_err() {
            reply.error(Errno::EIO);
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

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        self.touch();
        reply.ok(); // single-user container: whoever mounted it gets full access
    }
}
