//! Control socket of a mount whose connection holds its locks exclusively
//! (see db::open_db_for_mount). Nothing else can open the container then, so
//! `coffer backup` asks the mount daemon to make the copy with its own
//! connection instead.
//!
//! One request per connection: `backup\0DEST\0PASSWORD\0`, then the client
//! shuts down its write side; the answer is a single `ok ...` or
//! `error MESSAGE` line. The socket lives in a directory only its owner can
//! enter, and the daemon additionally checks the peer's uid - the copy stays
//! encrypted either way, but nobody else gets to have files written as you.

use crate::db;
use anyhow::{bail, Context, Result};
use rusqlite::{ffi, Connection};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zeroize::Zeroizing;

/// Pages copied per lock hold: 1024 x 4 KiB = 4 MiB, a few milliseconds of
/// decrypt + re-encrypt. Between steps the FUSE loop gets the connection
/// back, so a backup of a large container slows the mount down instead of
/// freezing it for minutes. Writes made in between through the same
/// connection are carried into the copy by SQLite itself.
const PAGES_PER_STEP: i32 = 1024;

/// Where the socket may live, in order: /run/user/UID (a per-user tmpfs
/// that logind creates) and /tmp/coffer-UID for systems or sessions without
/// one. The daemon binds in the first that works; the client tries all of
/// them, so a backup from cron (where /run/user/UID may be missing) still
/// finds a mount made from a desktop session.
fn candidate_dirs() -> Vec<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let mut dirs = Vec::new();
    let run = PathBuf::from(format!("/run/user/{uid}"));
    if run.is_dir() {
        dirs.push(run.join("coffer"));
    }
    dirs.push(PathBuf::from(format!("/tmp/coffer-{uid}")));
    dirs
}

/// Creates `dir` if needed and makes sure it's ours and closed to everyone
/// else - /tmp is shared, so a directory someone else created there first
/// must not be used.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
    }
    let md = std::fs::symlink_metadata(dir)?;
    if !md.is_dir() || md.uid() != unsafe { libc::getuid() } {
        bail!("{} is not a directory owned by you", dir.display());
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Socket file name for a container: FNV-1a of its canonical path. A fixed
/// algorithm on purpose - std's DefaultHasher may change between Rust
/// releases, and a client built later must still find an older daemon.
fn socket_name(container: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in container.as_os_str().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}.sock")
}

#[cfg(target_os = "linux")]
fn peer_uid(s: &UnixStream) -> Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("reading the peer's credentials");
    }
    Ok(cred.uid)
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(s: &UnixStream) -> Result<u32> {
    let (mut uid, mut gid) = (0, 0);
    if unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(std::io::Error::last_os_error()).context("reading the peer's credentials");
    }
    Ok(uid)
}

/// A bound socket, not yet served. Bound before the daemon forks so a
/// failure can still be reported on the terminal; served after it, because
/// threads don't survive the fork.
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
}

/// Removes the socket file when the mount ends. After a kill -9 it stays
/// behind, harmlessly: connecting to it fails, and the next mount of the
/// same container replaces it.
pub struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn bind(container: &Path) -> Result<Server> {
    let mut last_err = None;
    for dir in candidate_dirs() {
        if let Err(e) = ensure_private_dir(&dir) {
            last_err = Some(e);
            continue;
        }
        let path = dir.join(socket_name(container));
        // Anything already there is stale: a live daemon for this container
        // would hold lock_exclusive(), which the caller holds now.
        let _ = std::fs::remove_file(&path);
        match UnixListener::bind(&path) {
            Ok(listener) => return Ok(Server { listener, path }),
            Err(e) => last_err = Some(anyhow::Error::new(e).context(format!("binding {}", path.display()))),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no directory for the control socket")))
}

impl Server {
    pub fn spawn(self, con: Arc<Mutex<Connection>>) -> SocketGuard {
        let guard = SocketGuard(self.path.clone());
        let scratch_dir = self.path.parent().map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir);
        std::thread::spawn(move || {
            // One request at a time: a second backup simply waits for the first.
            for stream in self.listener.incoming().flatten() {
                handle(stream, &con, &scratch_dir);
            }
        });
        guard
    }
}

fn handle(mut stream: UnixStream, con: &Mutex<Connection>, scratch_dir: &Path) {
    let reply = match serve(&mut stream, con, scratch_dir) {
        Ok(()) => "ok\n".to_string(),
        Err(e) => format!("error {}\n", format!("{e:#}").replace('\n', " ")),
    };
    let _ = stream.write_all(reply.as_bytes());
}

fn serve(stream: &mut UnixStream, con: &Mutex<Connection>, scratch_dir: &Path) -> Result<()> {
    if peer_uid(stream)? != unsafe { libc::getuid() } {
        bail!("refused: only the user who mounted the container may use this socket");
    }
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut req = Zeroizing::new(Vec::new());
    stream.take(64 * 1024).read_to_end(&mut req)?;
    let fields: Vec<&[u8]> = req.split(|b| *b == 0).collect();
    let [b"backup", dest, password, b""] = fields.as_slice() else {
        bail!("malformed request");
    };
    let dest = PathBuf::from(std::ffi::OsString::from_vec(dest.to_vec()));
    if !dest.is_absolute() {
        bail!("malformed request: destination must be an absolute path");
    }
    let password = std::str::from_utf8(password).context("malformed request")?;
    let page = db::read_first_page(&con.lock().unwrap())?;
    if !db::verify_password(&page, password, scratch_dir)? {
        bail!("wrong password");
    }
    backup(con, &dest, password)
}

fn errmsg(con: &Connection) -> String {
    unsafe {
        let p = ffi::sqlite3_errmsg(con.handle());
        if p.is_null() {
            return "unknown error".into();
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// SQLite's online backup, driven by hand rather than through
/// rusqlite::backup::Backup: that one borrows the source connection for its
/// whole lifetime, which would mean holding the mount's mutex - and so
/// stalling every FUSE call - for the entire copy. Here the mutex is held
/// for one step at a time. The source is only ever touched with the mutex
/// held, the destination only by this thread.
fn backup(con: &Mutex<Connection>, dest: &Path, password: &str) -> Result<()> {
    let dest_con = Connection::open(dest).with_context(|| format!("creating {}", dest.display()))?;
    dest_con.execute_batch(&db::pragma_key_sql("key", password))?;
    let main = c"main";
    let b = {
        let src = con.lock().unwrap();
        unsafe { ffi::sqlite3_backup_init(dest_con.handle(), main.as_ptr(), src.handle(), main.as_ptr()) }
    };
    if b.is_null() {
        bail!("starting the backup: {}", errmsg(&dest_con));
    }
    let rc = loop {
        let rc = {
            let _src = con.lock().unwrap();
            unsafe { ffi::sqlite3_backup_step(b, PAGES_PER_STEP) }
        };
        match rc {
            ffi::SQLITE_OK => continue,
            ffi::SQLITE_BUSY | ffi::SQLITE_LOCKED => std::thread::sleep(Duration::from_millis(20)),
            rc => break rc,
        }
    };
    let finish = {
        let _src = con.lock().unwrap();
        unsafe { ffi::sqlite3_backup_finish(b) }
    };
    if rc != ffi::SQLITE_DONE || finish != ffi::SQLITE_OK {
        bail!("backup failed: {}", errmsg(&dest_con));
    }
    drop(dest_con);
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Client side: if a mount daemon for `container` is listening on this
/// host, has it write the backup and returns its outcome. None means no
/// daemon answered - not mounted here, or mounted with normal locking -
/// and the caller backs up directly, as before.
pub fn request_backup(container: &Path, dest: &Path, password: &str) -> Option<Result<()>> {
    let name = socket_name(container);
    let mut stream = candidate_dirs().into_iter().find_map(|d| UnixStream::connect(d.join(&name)).ok())?;
    Some((|| {
        let mut req = Zeroizing::new(Vec::new());
        req.extend_from_slice(b"backup\0");
        req.extend_from_slice(dest.as_os_str().as_bytes());
        req.push(0);
        req.extend_from_slice(password.as_bytes());
        req.push(0);
        stream.write_all(&req)?;
        stream.shutdown(Shutdown::Write)?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).context("waiting for the mount daemon")?;
        match line.trim_end() {
            "ok" => Ok(()),
            l if l.starts_with("error ") => bail!("{}", &l["error ".len()..]),
            _ => bail!("the mount daemon closed the connection without an answer"),
        }
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon side end to end, minus FUSE: a writer keeps using the
    /// exclusive connection while the backup runs in steps, and the copy
    /// opens with the password and contains everything.
    #[test]
    fn backup_through_an_exclusive_connection() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("s.coffer");
        db::create_container(&src, "pw", 0).unwrap();
        let con = db::open_db_for_mount(&src, "pw", false, true).unwrap();
        con.execute_batch("CREATE TABLE filler(x BLOB);").unwrap();
        for _ in 0..3000 {
            con.execute("INSERT INTO filler VALUES (randomblob(4000))", []).unwrap();
        }
        let con = Arc::new(Mutex::new(con));
        let writer = {
            let con = con.clone();
            std::thread::spawn(move || {
                for _ in 0..200 {
                    con.lock().unwrap().execute("INSERT INTO filler VALUES (randomblob(4000))", []).unwrap();
                }
            })
        };
        let dest = dir.path().join("b.coffer");
        backup(&con, &dest, "pw").unwrap();
        writer.join().unwrap();
        let n_src: i64 = con.lock().unwrap().query_row("SELECT count(*) FROM filler", [], |r| r.get(0)).unwrap();
        drop(con);
        let copy = db::open_db(&dest, "pw", true).unwrap();
        let n_copy: i64 = copy.query_row("SELECT count(*) FROM filler", [], |r| r.get(0)).unwrap();
        assert_eq!(n_copy, n_src, "writes made during the backup must be in the copy");
        assert!(db::open_db(&dest, "not-pw", true).is_err());
    }

    #[test]
    fn socket_name_is_stable() {
        assert_eq!(socket_name(Path::new("/home/u/v.coffer")), socket_name(Path::new("/home/u/v.coffer")));
        assert_ne!(socket_name(Path::new("/a")), socket_name(Path::new("/b")));
        // Pinned: a changed algorithm would strand clients of running daemons.
        assert_eq!(socket_name(Path::new("")), "cbf29ce484222325.sock");
    }
}
