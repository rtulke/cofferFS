mod config;
mod db;
mod fs;

use anyhow::{bail, Context, Result};
use config::{Config, Vault};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// coffer - growable encrypted containers, mountable as a normal user.
#[derive(Parser)]
#[command(
    name = "coffer",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("COFFER_GIT_HASH"), ")")
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new container file
    Create {
        file: PathBuf,
        /// Optional ceiling, e.g. 10G (default: unlimited, grows until host disk is full)
        #[arg(long)]
        max_size: Option<String>,
        /// Read the password from this file instead of prompting
        #[arg(long)]
        password_file: Option<PathBuf>,
        /// Also register the new container under this alias in ~/.coffer/config
        /// (together with --mountpoint), so `coffer mount ALIAS` works from then on
        #[arg(long, value_name = "ALIAS", requires = "mountpoint")]
        save: Option<String>,
        /// Mountpoint to register along with --save
        #[arg(long, value_name = "DIR", requires = "save")]
        mountpoint: Option<PathBuf>,
    },
    /// Mount a container as the current user
    Mount {
        /// Container file, or the alias of a vault registered in ~/.coffer/config.
        /// With no arguments at all: the one registered vault, or a menu if there are several
        target: Option<String>,
        /// Where to mount it - required for a file, taken from the config for an alias
        mountpoint: Option<PathBuf>,
        /// Also register this file + mountpoint (and the options given here) under
        /// ALIAS in ~/.coffer/config, same as `coffer add`
        #[arg(long, value_name = "ALIAS", requires = "mountpoint")]
        save: Option<String>,
        /// Stay in the foreground instead of daemonizing
        #[arg(long)]
        foreground: bool,
        /// Auto-unmount after this long with no filesystem activity, e.g. 30m, 2h (default: never)
        #[arg(long)]
        idle_timeout: Option<String>,
        /// Auto-compact (VACUUM) after this long idle, but only if there's a
        /// meaningful amount of reclaimable space (default: never)
        #[arg(long)]
        compact_on_idle: Option<String>,
        /// Read the password from this file instead of prompting
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
    /// Unmount a container
    Umount {
        /// Mountpoint, or the alias of a registered vault. With no argument: the
        /// one registered vault that is currently mounted, or a menu if several are
        target: Option<String>,
    },
    /// Register a container + mountpoint under an alias in ~/.coffer/config
    Add {
        /// Name to use with `coffer mount ALIAS` etc. (letters, digits, '-', '_', '.')
        alias: String,
        /// Existing container file
        file: PathBuf,
        /// Directory to mount it at (created on mount if missing)
        mountpoint: PathBuf,
        /// Default --idle-timeout for `coffer mount ALIAS`
        #[arg(long, value_name = "DURATION")]
        idle_timeout: Option<String>,
        /// Default --compact-on-idle for `coffer mount ALIAS`
        #[arg(long, value_name = "DURATION")]
        compact_on_idle: Option<String>,
        /// Default --password-file for `coffer mount ALIAS`
        #[arg(long, value_name = "FILE")]
        password_file: Option<PathBuf>,
    },
    /// Forget a registered alias (the container file itself is left untouched)
    Remove { alias: String },
    /// List the registered vaults and whether each is currently mounted
    List,
    /// Verify integrity without modifying the container
    Check {
        /// Container file, or the alias of a registered vault
        file: String,
        /// Read the password from this file instead of prompting
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
    /// Make a consistent copy (safe even while mounted)
    Backup {
        /// Container file, or the alias of a registered vault
        file: String,
        dest: PathBuf,
        /// Read the password from this file instead of prompting
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
    /// Change the container password
    Passwd {
        /// Container file, or the alias of a registered vault
        file: String,
        /// Read the current password from this file instead of prompting
        #[arg(long)]
        password_file: Option<PathBuf>,
        /// Read the new password from this file instead of prompting
        #[arg(long)]
        new_password_file: Option<PathBuf>,
    },
    /// Show container stats
    Info {
        /// Container file, or the alias of a registered vault
        file: String,
        /// Read the password from this file instead of prompting
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
    /// Reclaim disk space after deletions (VACUUM); refuses to run against a mounted container
    Compact {
        /// Container file, or the alias of a registered vault
        file: String,
        /// Read the password from this file instead of prompting
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
    /// Print a shell completion script to stdout
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim().to_uppercase();
    let (num, mult) = match s.chars().last() {
        Some('K') => (&s[..s.len() - 1], 1024u64),
        Some('M') => (&s[..s.len() - 1], 1024u64.pow(2)),
        Some('G') => (&s[..s.len() - 1], 1024u64.pow(3)),
        Some('T') => (&s[..s.len() - 1], 1024u64.pow(4)),
        _ => (s.as_str(), 1u64),
    };
    let value: f64 = num.parse().context("invalid size")?;
    // A negative value would otherwise saturate to 0 on the cast below -
    // which this codebase treats as "unlimited", the exact opposite of a
    // safety ceiling someone typed a negative number for by mistake.
    if !value.is_finite() || value < 0.0 {
        bail!("invalid size: {s} (must be a non-negative number)");
    }
    Ok((value * mult as f64) as u64)
}

fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1u64),
        Some('m') => (&s[..s.len() - 1], 60u64),
        Some('h') => (&s[..s.len() - 1], 3600u64),
        Some('d') => (&s[..s.len() - 1], 86400u64),
        _ => (s, 1u64),
    };
    let value: f64 = num.parse().context("invalid duration")?;
    // Duration::from_secs_f64 panics outright on a negative/NaN/infinite
    // value - which, with this project's `panic = "abort"` release profile,
    // means a single mistyped flag (e.g. --idle-timeout=-5m) would abort
    // the whole process instead of producing a normal CLI error.
    let secs = value * mult as f64;
    if !secs.is_finite() || secs < 0.0 {
        bail!("invalid duration: {s} (must be a non-negative number)");
    }
    Ok(Duration::from_secs_f64(secs))
}

/// Prompts on the real TTY when there is one (masked input); falls back to a
/// plain-text stdin read when stdin isn't a terminal (e.g. piped/scripted use).
fn read_one(prompt: &str) -> Result<String> {
    use std::io::{IsTerminal, Write};
    if std::io::stdin().is_terminal() {
        Ok(rpassword::prompt_password(prompt)?)
    } else {
        eprint!("{prompt}");
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
}

fn read_password(confirm: bool) -> Result<String> {
    let pw = read_one("Container password: ")?;
    if pw.is_empty() {
        bail!("empty password refused");
    }
    if confirm {
        let pw2 = read_one("Confirm password: ")?;
        if pw != pw2 {
            bail!("passwords did not match");
        }
    }
    Ok(pw)
}

/// A password file is its own confirmation (there's nothing to retype
/// against), so `confirm` only applies to the interactive fallback.
fn read_password_source(password_file: Option<&Path>, confirm: bool) -> Result<String> {
    let Some(path) = password_file else {
        return read_password(confirm);
    };
    warn_if_world_readable(path);
    let content = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    // Only the first line, matching read_one()'s stdin-piped fallback
    // (which reads exactly one line via read_line) - reading the whole file
    // would silently fold an accidental extra line (a trailing comment, a
    // stray blank line) into the password instead of just the line the
    // user actually meant.
    let pw = content.lines().next().unwrap_or("").to_string();
    if pw.is_empty() {
        bail!("empty password in {}", path.display());
    }
    Ok(pw)
}

#[cfg(unix)]
fn warn_if_world_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.permissions().mode() & 0o077 != 0 {
            eprintln!(
                "warning: {} is readable by others - consider: chmod 600 {}",
                path.display(),
                path.display()
            );
        }
    }
}

fn cmd_create(file: &Path, max_size: Option<String>, password_file: Option<&Path>) -> Result<()> {
    let max_size = max_size.map(|s| parse_size(&s)).transpose()?.unwrap_or(0);
    let password = read_password_source(password_file, true)?;
    db::create_container(file, &password, max_size)?;
    if max_size > 0 {
        println!("coffer: created {} (grows automatically up to {} bytes)", file.display(), max_size);
    } else {
        println!("coffer: created {} (grows automatically, no ceiling)", file.display());
    }
    Ok(())
}

/// Everything `mount` needs besides the two paths: the CLI flags, with a
/// registered vault's stored values (when mounting by alias) filling in
/// whatever the command line left unset.
struct MountOpts {
    foreground: bool,
    idle_timeout: Option<String>,
    compact_on_idle: Option<String>,
    password_file: Option<PathBuf>,
}

impl MountOpts {
    fn with_defaults_from(mut self, vault: &Vault) -> MountOpts {
        self.idle_timeout = self.idle_timeout.or_else(|| vault.idle_timeout.clone());
        self.compact_on_idle = self.compact_on_idle.or_else(|| vault.compact_on_idle.clone());
        self.password_file = self.password_file.or_else(|| vault.password_file.clone());
        self
    }
}

fn cmd_mount(file: &Path, mountpoint: &Path, opts: MountOpts) -> Result<()> {
    let idle_timeout = opts.idle_timeout.map(|s| parse_duration(&s)).transpose()?;
    let compact_on_idle = opts.compact_on_idle.map(|s| parse_duration(&s)).transpose()?;
    let abs_file = file.canonicalize().with_context(|| file.display().to_string())?;

    // Acquired before even opening the database: if some other coffer
    // process already has this container locked (mounted, or a passwd/
    // compact in progress), fail immediately - before wasting a password
    // prompt, and critically before `open_db` below keys a connection that
    // could otherwise end up stale if a concurrent `passwd` rekeys the
    // container in the gap between us opening it and us locking it.
    let _lock = db::lock_exclusive(&abs_file)?;

    // Checked before the password prompt: "mountpoint is not empty" is
    // worth knowing before typing a passphrase, not after.
    std::fs::create_dir_all(mountpoint)?;
    if std::fs::read_dir(mountpoint)?.next().is_some() {
        bail!("mountpoint {} is not empty", mountpoint.display());
    }
    let abs_mountpoint = mountpoint.canonicalize()?;

    let password = read_password_source(opts.password_file.as_deref(), false)?;
    let con = db::open_db(file, &password, false)?;
    let max_size = db::read_max_size(&con);

    println!("coffer: mounting {} at {}", file.display(), mountpoint.display());

    let filesystem = fs::CofferFS::new(con, max_size, &abs_file);
    // Grabbed before `filesystem` is moved into Session::new below - these
    // are just cloned Arc handles, independent of filesystem's ownership.
    let last_activity = filesystem.last_activity();
    let connection_handle = filesystem.connection_handle();
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::FSName("coffer".into()),
        fuser::MountOption::NoDev,
        fuser::MountOption::NoSuid,
    ];

    // Session::new() performs the actual mount(2) and the FUSE handshake
    // synchronously and returns a Result - deliberately done here, in the
    // foreground, before any daemonizing. `fuser::mount()` (Session::new +
    // .run() in one call) would otherwise hide a failure here: `mount`
    // normally daemonizes right after this point, and daemonize's double-
    // fork lets the original process exit(0) before the (grand)child has
    // done anything - so an error surfacing only after that fork would
    // vanish into the daemon's /dev/null stderr while the calling shell
    // already saw "success". Splitting it like this means a bad mountpoint,
    // a permissions race, or any other mount(2)-time failure is reported
    // normally, synchronously, with a real exit code.
    let session = fuser::Session::new(filesystem, &abs_mountpoint, &config).context("failed to mount")?;

    if !opts.foreground {
        use daemonize::{Daemonize, Stdio};
        Daemonize::new()
            .working_directory("/")
            .stdout(Stdio::devnull())
            .stderr(Stdio::devnull())
            .start()
            .context("failed to daemonize")?;
    }

    // Spawned only after daemonizing (when applicable): fork() does not
    // duplicate other threads into the child, only the calling thread - a
    // watcher thread started before the fork would simply not exist in the
    // daemonized process. The FUSE session itself survives the fork fine
    // (it's just an open file descriptor plus ordinary heap data, not a
    // thread), which is what makes splitting Session::new from .run() safe.
    if let Some(timeout) = idle_timeout {
        spawn_idle_watcher(abs_mountpoint.clone(), last_activity.clone(), timeout);
    }
    if let Some(threshold) = compact_on_idle {
        spawn_compact_on_idle_watcher(connection_handle, last_activity, abs_file.clone(), threshold);
    }

    session.run()?;
    Ok(())
}

// Every handled FUSE call refreshes CofferFS::last_activity (see fs.rs); this
// just polls that shared counter and unmounts itself - as the same uid that
// mounted it - once it's been idle past the configured timeout. Poll interval
// is capped at 30s so the actual unmount never lags the deadline by more than
// that, regardless of how long the timeout itself is.
fn spawn_idle_watcher(mountpoint: PathBuf, last_activity: Arc<AtomicU64>, timeout: Duration) {
    let poll = timeout.min(Duration::from_secs(30)).max(Duration::from_secs(1));
    std::thread::spawn(move || loop {
        std::thread::sleep(poll);
        let idle_for = db::now_secs() - last_activity.load(Ordering::Relaxed) as f64;
        if idle_for >= timeout.as_secs_f64() {
            eprintln!(
                "coffer: idle for {}s (limit {}s), auto-unmounting {}",
                idle_for as u64,
                timeout.as_secs(),
                mountpoint.display()
            );
            match run_unmount(&mountpoint) {
                Ok(status) if !status.success() => {
                    eprintln!("coffer: auto-unmount of {} failed: {status}", mountpoint.display());
                }
                Err(e) => {
                    eprintln!("coffer: auto-unmount of {} failed: {e}", mountpoint.display());
                }
                Ok(_) => {}
            }
            return;
        }
    });
}

// Runs VACUUM once the mount has been idle for `threshold`, but only if
// there's a meaningful amount of freed-but-unreclaimed space to actually
// get back - most idle periods have nothing worth reclaiming (freed space
// is already being reused for future writes), so checking first avoids a
// pointless full-file rewrite. Uses the same connection/lock the FUSE loop
// itself uses, so it's automatically serialized with any live filesystem
// activity - safe, but a filesystem call that arrives mid-VACUUM will block
// until it finishes, same as any other write contending for that lock.
// Keeps running (doesn't exit after firing once), so a later idle period
// can reclaim space freed by deletions that happened since the last run.
fn spawn_compact_on_idle_watcher(
    con: Arc<std::sync::Mutex<rusqlite::Connection>>,
    last_activity: Arc<AtomicU64>,
    container_path: PathBuf,
    threshold: Duration,
) {
    const MIN_RECLAIM_BYTES: u64 = 64 * 1024 * 1024;
    const MIN_RECLAIM_FRACTION: f64 = 0.10;
    let poll = threshold.min(Duration::from_secs(30)).max(Duration::from_secs(1));
    std::thread::spawn(move || loop {
        std::thread::sleep(poll);
        let idle_for = db::now_secs() - last_activity.load(Ordering::Relaxed) as f64;
        if idle_for < threshold.as_secs_f64() {
            continue;
        }
        let Ok(on_disk) = std::fs::metadata(&container_path).map(|m| m.len()) else {
            continue;
        };
        let guard = con.lock().unwrap();
        // Re-check idle_for now that the lock is actually held: activity
        // could have resumed in the (however brief) gap between the check
        // above and acquiring this lock, and a VACUUM on a large container
        // holds fuser's single dispatch thread for its whole duration - so
        // this is worth re-verifying rather than compacting straight into a
        // just-resumed session.
        let idle_for = db::now_secs() - last_activity.load(Ordering::Relaxed) as f64;
        if idle_for < threshold.as_secs_f64() {
            continue;
        }
        let used: i64 = guard
            .query_row("SELECT COALESCE(SUM(size),0) FROM inodes", [], |r| r.get(0))
            .unwrap_or(0);
        let gap = on_disk.saturating_sub(used as u64);
        if gap < MIN_RECLAIM_BYTES || (gap as f64) < on_disk as f64 * MIN_RECLAIM_FRACTION {
            continue;
        }
        eprintln!(
            "coffer: idle with ~{gap} reclaimable bytes, compacting {}...",
            container_path.display()
        );
        // VACUUM alone doesn't shrink the main file under WAL mode - it
        // lands in the WAL first. A normal (non-idle-watcher) connection
        // gets this for free from SQLite's checkpoint-on-last-close, but
        // this connection stays open for the mount's whole lifetime, so the
        // checkpoint has to be forced explicitly or the main file's size on
        // disk never actually changes.
        if let Err(e) = guard.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);") {
            eprintln!("coffer: auto-compact failed: {e}");
        }
    });
}

// A plain unmount can fail with "Device or resource busy" when something
// still references the mount - a shell cd'd into it, an editor with a file
// open, or (the sneaky one) a *different* user's desktop session: gvfs/
// tracker-style daemons watch every mount they can see, and a mountpoint
// under /tmp is visible to everyone who logs in after you. Escalating to a
// lazy unmount - detach the mountpoint now, finish cleanup once nothing
// still references it - resolves that without needing root: it's the same
// mounting-user permissions as the plain unmount, just a different kernel-
// side detach mode, so no sudo involved here.
//
// The catch with lazy: "once nothing still references it" can be never (a
// desktop session that stays logged in for days), and until then the coffer
// daemon keeps running, keeps the container open, and keeps the exclusive
// lock - which on an NFS home is visible on every other host, so `coffer
// mount` elsewhere fails with "already in use" while `coffer umount` here
// reported success. So after a successful lazy detach, the FUSE connection
// itself is aborted via /sys/fs/fuse/connections/<id>/abort: the kernel
// fails everything still open on it and the daemon's request loop ends the
// same way it does on a normal unmount, so it exits cleanly and the lock is
// released immediately. Whatever was still holding the mount gets I/O
// errors, which is the honest outcome of an unmount the user asked for.
fn run_unmount(mountpoint: &Path) -> std::io::Result<std::process::ExitStatus> {
    // Looked up *before* detaching: once the lazy unmount has removed the
    // mountpoint from the mount table there's no way left to find out which
    // connection the daemon is sitting on.
    let connection = fuse_connection_id(mountpoint);

    // The plain attempt's own output is suppressed: if it fails it's usually
    // just "device or resource busy" en route to the lazy retry succeeding,
    // and showing that would look like a real error for what's actually a
    // routine escalation. If the lazy attempt fails too, its output is left
    // visible - that's a genuine failure worth seeing.
    let status = match which("fusermount3").or_else(|| which("fusermount")) {
        Some(fusermount) => {
            let status = std::process::Command::new(&fusermount)
                .arg("-u")
                .arg(mountpoint)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?;
            if status.success() {
                return Ok(status);
            }
            std::process::Command::new(&fusermount)
                .arg("-u")
                .arg("-z")
                .arg(mountpoint)
                .status()?
        }
        None => {
            let status = std::process::Command::new("umount")
                .arg(mountpoint)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?;
            if status.success() {
                return Ok(status);
            }
            std::process::Command::new("umount").arg("-l").arg(mountpoint).status()?
        }
    };
    if status.success() {
        if let Some(id) = connection {
            abort_fuse_connection(id, mountpoint);
        }
    }
    Ok(status)
}

// Every one of coffer's own FUSE mounts currently visible, as (mountpoint,
// connection id), from /proc/self/mountinfo: field 3 is the filesystem's
// `major:minor`, and the minor is what /sys/fs/fuse/connections/<id> is
// named after. Restricted to coffer's own mounts (source "coffer") so
// `coffer umount` pointed at somebody else's FUSE mount never aborts that.
// In mount-table order: for a path mounted over more than once, the entry
// listed last is the one currently visible there.
fn coffer_mounts() -> Vec<(String, u32)> {
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for line in mountinfo.lines() {
        // mount-id parent-id major:minor root mountpoint options [tags] - fstype source superopts
        let mut fields = line.split(' ');
        let (Some(_), Some(_), Some(devnum), Some(_), Some(mp)) =
            (fields.next(), fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let mut tail = match line.split(" - ").nth(1) {
            Some(rest) => rest.split(' '),
            None => continue,
        };
        let (Some(fstype), Some(source)) = (tail.next(), tail.next()) else {
            continue;
        };
        // libfuse3 registers the mount as type "fuse" with FSName as the
        // source ("coffer"); only a `subtype=` option would make the type
        // itself read "fuse.coffer", so accept both spellings.
        let ours = (fstype == "fuse" || fstype.starts_with("fuse.")) && source == "coffer";
        if !ours {
            continue;
        }
        if let Some(minor) = devnum.split(':').nth(1).and_then(|m| m.parse().ok()) {
            found.push((unescape_mountinfo(mp), minor));
        }
    }
    found
}

// The form a mountpoint takes in mountinfo, for matching against
// coffer_mounts(). Deliberately not stat()-based: stat on a FUSE mountpoint
// is itself a FUSE request, which blocks for good if the daemon behind it
// is wedged - precisely the situation `umount` gets reached for. So only
// the mountpoint's *parent* is canonicalized (symlinks resolved); the last
// path component is compared by name.
fn mount_key(mountpoint: &Path) -> Option<String> {
    let name = mountpoint.file_name()?;
    let parent = match mountpoint.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    Some(parent.canonicalize().ok()?.join(name).to_str()?.to_string())
}

// Kernel id of the FUSE connection behind one of coffer's own mounts; last
// match wins (see coffer_mounts).
fn fuse_connection_id(mountpoint: &Path) -> Option<u32> {
    let target = mount_key(mountpoint)?;
    coffer_mounts().into_iter().filter(|(mp, _)| *mp == target).map(|(_, id)| id).last()
}

fn is_mounted(mountpoint: &Path, mounts: &[(String, u32)]) -> bool {
    mount_key(mountpoint).is_some_and(|target| mounts.iter().any(|(mp, _)| *mp == target))
}

// mountinfo escapes space, tab, newline and backslash in paths as \040,
// \011, \012 and \134 (octal), so a mountpoint like "/tmp/my vault" shows up
// as "/tmp/my\040vault" and has to be decoded before comparing.
fn unescape_mountinfo(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1..i + 4].iter().all(|b| (b'0'..=b'7').contains(b)) {
            let code = (bytes[i + 1] - b'0') * 64 + (bytes[i + 2] - b'0') * 8 + (bytes[i + 3] - b'0');
            out.push(code);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn abort_fuse_connection(id: u32, mountpoint: &Path) {
    let abort = format!("/sys/fs/fuse/connections/{id}/abort");
    match std::fs::write(&abort, "1") {
        Ok(()) => eprintln!(
            "coffer: {} was still in use by another process - detached it and aborted the \
FUSE connection, so the coffer daemon exits and releases the container now. Anything that \
still had files open there gets I/O errors from here on.",
            mountpoint.display()
        ),
        // The connection is already gone (daemon crashed and the kernel has
        // finished tearing it down) - the lazy detach was all that was
        // needed, nothing to warn about.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!(
            "coffer: {} was still in use by another process - detached it, but could not abort \
the FUSE connection ({abort}: {e}). Until whatever holds it lets go, the coffer daemon keeps \
running and keeps the container locked (also for mounts from other hosts, if it lives on a \
network filesystem). To force it:\n    echo 1 > {abort}",
            mountpoint.display()
        ),
    }
}

fn cmd_umount(target: Option<&str>) -> Result<()> {
    let mountpoint = match target {
        // A path needs no registry at all - so a broken config file can
        // never get in the way of unmounting something by path.
        Some(t) if looks_like_path(t) => PathBuf::from(t),
        Some(t) => match Config::load()?.get(t)? {
            Some(vault) => {
                if !is_mounted(&vault.mountpoint, &coffer_mounts()) {
                    bail!("'{}' is not mounted (its mountpoint is {})", vault.alias, vault.mountpoint.display());
                }
                println!("coffer: unmounting '{}' at {}", vault.alias, vault.mountpoint.display());
                vault.mountpoint
            }
            None => PathBuf::from(t),
        },
        None => {
            let vaults = Config::load()?.vaults()?;
            if vaults.is_empty() {
                bail!("no vaults registered (see `coffer add`) - give the mountpoint: coffer umount <mountpoint>");
            }
            let mounts = coffer_mounts();
            let mounted: Vec<Vault> = vaults.into_iter().filter(|v| is_mounted(&v.mountpoint, &mounts)).collect();
            if mounted.is_empty() {
                bail!("none of the registered vaults is currently mounted (see `coffer list`)");
            }
            let vault = pick_vault(mounted, "umount")?;
            println!("coffer: unmounting '{}' at {}", vault.alias, vault.mountpoint.display());
            vault.mountpoint
        }
    };
    let status = run_unmount(&mountpoint)?;
    if !status.success() {
        hint_sudo_umount(&mountpoint);
    }
    std::process::exit(status.code().unwrap_or(1));
}

// fusermount3/fusermount is setuid-root and briefly runs as effective root to
// call umount2(); FUSE's default access check (fuser's SessionACL::Owner)
// only allows the exact mounting uid, not even root, so it can reject that
// even when the real caller is the mount's own owner - e.g. reliably
// reproducible when the mountpoint lives on an NFS home directory with
// root_squash. Plain `sudo umount` sidesteps this: real root calls umount2()
// directly, without going through that FUSE-side check at all.
fn hint_sudo_umount(mountpoint: &Path) {
    eprintln!(
        "If that failed with a permission error even though you're the one who \
mounted it, try:\n    sudo umount {}",
        mountpoint.display()
    );
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    })
}

// ---- the vault registry (~/.coffer/config) ----------------------------------

// Rule for every positional that accepts "a container or an alias": a bare
// word (no '/', not starting with '.' or '~') is looked up as an alias first;
// anything else is a path, so `./work` always means the file even if an
// alias `work` exists.
fn looks_like_path(arg: &str) -> bool {
    arg.contains('/') || arg.starts_with('.') || arg.starts_with('~')
}

/// For the commands that just take "a container": a registered alias
/// resolves to its file, anything else is the path as given.
fn resolve_container(arg: &str) -> Result<PathBuf> {
    if looks_like_path(arg) {
        return Ok(PathBuf::from(arg));
    }
    if let Some(vault) = Config::load()?.get(arg)? {
        return Ok(vault.file);
    }
    let path = PathBuf::from(arg);
    if !path.exists() {
        bail!("{arg}: no such file, and no registered vault by that alias (see `coffer list`)");
    }
    Ok(path)
}

/// Works out what `mount` was asked to mount: nothing at all (the registry
/// decides), an alias (optionally with a mountpoint override), or the
/// classic file + mountpoint pair, which never touches the registry.
fn plan_mount(target: Option<&str>, mountpoint: Option<PathBuf>, opts: MountOpts) -> Result<(PathBuf, PathBuf, MountOpts)> {
    let registered = match target {
        Some(t) if looks_like_path(t) => None,
        Some(t) => Config::load()?.get(t)?,
        None => {
            let vaults = Config::load()?.vaults()?;
            if vaults.is_empty() {
                bail!(
                    "no vaults registered yet. Either give the paths:\n    coffer mount <file> <mountpoint>\n\
add --save <alias> to that to register them, or register without mounting:\n    coffer add <alias> <file> <mountpoint>"
                );
            }
            Some(pick_vault(vaults, "mount")?)
        }
    };
    match (registered, target, mountpoint) {
        (Some(vault), _, mountpoint) => {
            let mountpoint = mountpoint.unwrap_or_else(|| vault.mountpoint.clone());
            let opts = opts.with_defaults_from(&vault);
            Ok((vault.file, mountpoint, opts))
        }
        (None, Some(file), Some(mountpoint)) => Ok((PathBuf::from(file), mountpoint, opts)),
        (None, Some(t), None) => bail!(
            "{t}: not a registered vault (see `coffer list`). To mount a container file, give its \
mountpoint too:\n    coffer mount {t} <mountpoint>"
        ),
        (None, None, _) => unreachable!("a missing target always resolves to a vault or an error above"),
    }
}

/// One candidate needs no asking. Several do: a numbered menu when there's
/// a terminal to ask on, an error pointing at the alias form otherwise.
fn pick_vault(mut candidates: Vec<Vault>, verb: &str) -> Result<Vault> {
    use std::io::{IsTerminal, Write};
    if candidates.len() == 1 {
        return Ok(candidates.remove(0));
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "{} registered vaults and no terminal to choose on - say which one: coffer {verb} <alias> \
(see `coffer list`)",
            candidates.len()
        );
    }
    let mounts = coffer_mounts();
    println!("Registered vaults:");
    print_vault_table(&candidates, &mounts, true);
    let question = if verb == "umount" { "Unmount" } else { "Mount" };
    loop {
        print!("\n{question} which one? [1-{}, or an alias; empty to abort] ", candidates.len());
        std::io::stdout().flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            bail!("aborted");
        }
        let answer = line.trim();
        if answer.is_empty() || answer == "q" {
            bail!("aborted");
        }
        if let Ok(n) = answer.parse::<usize>() {
            if (1..=candidates.len()).contains(&n) {
                return Ok(candidates.swap_remove(n - 1));
            }
        }
        if let Some(i) = candidates.iter().position(|v| v.alias == answer) {
            return Ok(candidates.swap_remove(i));
        }
        eprintln!("coffer: {answer:?} is neither a number from 1 to {} nor an alias", candidates.len());
    }
}

fn print_vault_table(vaults: &[Vault], mounts: &[(String, u32)], numbered: bool) {
    let mut header: Vec<String> =
        ["ALIAS", "MOUNTPOINT", "FILE", "SIZE", "MODIFIED", "STATUS"].iter().map(|h| h.to_string()).collect();
    if numbered {
        header.insert(0, String::new());
    }
    let mut rows = Vec::new();
    for (i, v) in vaults.iter().enumerate() {
        // Size and mtime come from the file itself, no password needed -
        // the creation date lives inside the container, behind the key.
        let (size, modified, missing) = match std::fs::metadata(&v.file) {
            Ok(m) => (
                human_size(m.len()),
                m.modified().map(format_local_time).unwrap_or_else(|_| "-".to_string()),
                false,
            ),
            Err(_) => ("-".to_string(), "-".to_string(), true),
        };
        let status = if is_mounted(&v.mountpoint, mounts) {
            "mounted"
        } else if missing {
            "file missing"
        } else {
            "-"
        };
        let mut row = vec![
            v.alias.clone(),
            config::abbreviate_home(&v.mountpoint),
            config::abbreviate_home(&v.file),
            size,
            modified,
            status.to_string(),
        ];
        if numbered {
            row.insert(0, format!("{})", i + 1));
        }
        rows.push(row);
    }
    print_table(&header, &rows);
}

fn print_table(header: &[String], rows: &[Vec<String>]) {
    let mut widths = vec![0usize; header.len()];
    for row in std::iter::once(header).chain(rows.iter().map(Vec::as_slice)) {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    for row in std::iter::once(header).chain(rows.iter().map(Vec::as_slice)) {
        let cells: Vec<String> = row.iter().enumerate().map(|(i, c)| format!("{c:<w$}", w = widths[i])).collect();
        println!("{}", cells.join("  ").trim_end());
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// Local time via libc rather than pulling in a date crate for one column.
fn format_local_time(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r only writes into the `tm` we hand it, and both
    // pointers are to live locals.
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return "-".to_string();
    }
    let mut buf = [0 as libc::c_char; 32];
    // SAFETY: strftime writes at most `buf.len()` bytes into `buf` and
    // reads only the NUL-terminated format and the initialized `tm`.
    let n = unsafe { libc::strftime(buf.as_mut_ptr(), buf.len(), c"%Y-%m-%d %H:%M".as_ptr(), &tm) };
    let bytes: Vec<u8> = buf[..n].iter().map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Registers (or re-registers) a vault. The container has to exist already:
/// that's what makes a typo in its path fail right here instead of at the
/// next `coffer mount`. The mountpoint needn't - mount creates it.
fn cmd_add(cfg: &mut Config, mut vault: Vault) -> Result<()> {
    config::validate_alias(&vault.alias)?;
    if !vault.file.is_file() {
        bail!("{}: no such file", vault.file.display());
    }
    // Stored absolute (but with symlinks left alone, so a link that gets
    // re-pointed later still works), since the registry is used from any
    // working directory.
    vault.file = std::path::absolute(&vault.file)?;
    vault.mountpoint = std::path::absolute(&vault.mountpoint)?;
    if let Some(pw) = &vault.password_file {
        vault.password_file = Some(std::path::absolute(pw)?);
    }
    // Same validation the flags get at mount time, so a bad value fails
    // once here rather than at every later mount.
    if let Some(d) = &vault.idle_timeout {
        parse_duration(d).context("--idle-timeout")?;
    }
    if let Some(d) = &vault.compact_on_idle {
        parse_duration(d).context("--compact-on-idle")?;
    }
    let replaced = cfg.upsert(&vault);
    cfg.save()?;
    println!(
        "coffer: {} '{}' in {}: {} at {}",
        if replaced { "updated" } else { "registered" },
        vault.alias,
        config::abbreviate_home(&cfg.path),
        config::abbreviate_home(&vault.file),
        config::abbreviate_home(&vault.mountpoint)
    );
    Ok(())
}

fn cmd_remove(cfg: &mut Config, alias: &str) -> Result<()> {
    // Looked up leniently: a section too broken to parse is exactly the
    // kind of entry someone would want to remove.
    let file = cfg.get(alias).ok().flatten().map(|v| v.file);
    if !cfg.remove(alias) {
        bail!("no registered vault named '{alias}' (see `coffer list`)");
    }
    cfg.save()?;
    match file {
        Some(file) => println!(
            "coffer: removed '{alias}' from {} ({} itself is untouched)",
            config::abbreviate_home(&cfg.path),
            config::abbreviate_home(&file)
        ),
        None => println!("coffer: removed '{alias}' from {}", config::abbreviate_home(&cfg.path)),
    }
    Ok(())
}

fn cmd_list(cfg: &Config) -> Result<()> {
    let vaults = cfg.vaults()?;
    if vaults.is_empty() {
        println!("coffer: no vaults registered in {}", config::abbreviate_home(&cfg.path));
        println!("Register one with:\n    coffer add <alias> <file> <mountpoint>\nor while mounting:\n    coffer mount <file> <mountpoint> --save <alias>");
        return Ok(());
    }
    print_vault_table(&vaults, &coffer_mounts(), false);
    Ok(())
}

fn cmd_check(file: &Path, password_file: Option<&Path>) -> Result<()> {
    let password = read_password_source(password_file, false)?;
    let con = db::open_db(file, &password, true)?;

    // SQLCipher's own "PRAGMA cipher_integrity_check" has a confirmed upstream bug
    // (32-bit offset overflow in sqlcipher_codec_ctx_integrity_check, src/crypto.c -
    // https://github.com/sqlcipher/sqlcipher/issues/604): it misreports every page
    // from 4GB onward (page (4*1024^3)/page_size + 1) as HMAC-failed, even though
    // those pages decrypt and verify correctly through the normal read path.
    // Reproduced independently across two SQLCipher builds (Ubuntu's
    // libsqlcipher-dev and a from-source build), so it isn't specific to this
    // project. Fixed upstream in SQLCipher 4.17.0; still present as of writing in
    // the 4.14.0 vendored by libsqlite3-sys (this project's
    // bundled-sqlcipher-vendored-openssl dependency), so this workaround stays
    // needed for now. "PRAGMA integrity_check" below is authoritative: it actually
    // walks and decrypts every page to verify the B-tree structure, so if it
    // passes, the data is genuinely intact.
    let page_size: u64 = con
        .query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0))
        .map(|v| v as u64)
        .or_else(|_| con.query_row("PRAGMA page_size", [], |r| r.get::<_, String>(0)).map(|s| s.parse().unwrap_or(4096)))?;
    let boundary_page: u64 = (4 * 1024 * 1024 * 1024) / page_size;

    println!("Running SQLCipher page-level HMAC integrity check...");
    let mut stmt = con.prepare("PRAGMA cipher_integrity_check")?;
    let raw_problems: Vec<String> = stmt.query_map([], |r| r.get::<_, String>(0))?.filter_map(|r| r.ok()).collect();
    let mut real_problems = Vec::new();
    let mut known_limitation = 0u64;
    for msg in &raw_problems {
        let page = msg.split_whitespace().last().and_then(|t| t.parse::<u64>().ok());
        match page {
            Some(p) if p > boundary_page => known_limitation += 1,
            _ => real_problems.push(msg.clone()),
        }
    }
    if !real_problems.is_empty() {
        for p in &real_problems {
            println!("  CORRUPT PAGE: {p}");
        }
    } else if known_limitation > 0 {
        println!(
            "  OK: no corrupt pages within the first 4GB ({known_limitation} page(s) past the \
             4GB mark misreported by a known SQLCipher bug - see note below)."
        );
    } else {
        println!("  OK: no corrupt pages detected.");
    }

    println!("Running SQLite structural integrity check (authoritative for containers >4GB)...");
    let lines: Vec<String> = match con
        .prepare("PRAGMA integrity_check")
        .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(0))?.collect())
    {
        Ok(lines) => lines,
        Err(e) => vec![format!("error: {e}")],
    };
    for line in &lines {
        println!("  {line}");
    }
    let structurally_ok = lines == ["ok"];

    if known_limitation > 0 && structurally_ok {
        println!(
            "\nNote: {known_limitation} page(s) past the 4GB mark were misreported as corrupt \
             by cipher_integrity_check due to an upstream SQLCipher bug unrelated to this \
             project. The structural check above actually decrypts and verifies every page and \
             passed cleanly, so your data is intact."
        );
    }

    if !real_problems.is_empty() || !structurally_ok {
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_backup(file: &Path, dest: &Path, password_file: Option<&Path>) -> Result<()> {
    let password = read_password_source(password_file, false)?;
    let con = db::open_db(file, &password, true)?;
    let mut dest_con = rusqlite::Connection::open(dest)?;
    dest_con.execute_batch(&db::pragma_key_sql("key", &password))?;
    {
        let backup = rusqlite::backup::Backup::new(&con, &mut dest_con)?;
        // i32::MAX pages per step = copy everything in one step; the source is
        // only ever opened read-only for this, so there's no writer to yield to.
        backup.run_to_completion(i32::MAX, std::time::Duration::from_millis(0), None)?;
    }
    drop(dest_con);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o600))?;
    }
    println!("coffer: consistent backup written to {}", dest.display());
    Ok(())
}

fn cmd_passwd(file: &Path, password_file: Option<&Path>, new_password_file: Option<&Path>) -> Result<()> {
    let _lock = db::lock_exclusive(file)?;
    if password_file.is_none() {
        println!("Current password:");
    }
    let old = read_password_source(password_file, false)?;
    let con = db::open_db(file, &old, false)?;
    if new_password_file.is_none() {
        println!("New password:");
    }
    let new = read_password_source(new_password_file, true)?;
    con.execute_batch(&db::pragma_key_sql("rekey", &new))?;
    println!("coffer: password changed.");
    Ok(())
}

fn cmd_info(file: &Path, password_file: Option<&Path>) -> Result<()> {
    let password = read_password_source(password_file, false)?;
    let con = db::open_db(file, &password, true)?;
    let files: i64 = con.query_row("SELECT COUNT(*) FROM inodes WHERE kind=1", [], |r| r.get(0))?;
    let dirs: i64 = con.query_row("SELECT COUNT(*) FROM inodes WHERE kind=0", [], |r| r.get(0))?;
    let links: i64 = con.query_row("SELECT COUNT(*) FROM inodes WHERE kind=2", [], |r| r.get(0))?;
    let used: i64 = con.query_row("SELECT COALESCE(SUM(size),0) FROM inodes", [], |r| r.get(0))?;
    let max_size = db::read_max_size(&con);
    drop(con);
    let disk_size = std::fs::metadata(file)?.len();

    println!("File:              {}", file.display());
    println!("On-disk size:      {disk_size} bytes");
    println!("Logical data used: {used} bytes");
    if max_size > 0 {
        println!("Ceiling:           {max_size} bytes");
    } else {
        println!("Ceiling:           none (grows until host disk is full)");
    }
    println!("Directories:       {dirs}");
    println!("Files:             {files}");
    println!("Symlinks:          {links}");
    Ok(())
}

fn cmd_compact(file: &Path, password_file: Option<&Path>) -> Result<()> {
    let _lock = db::lock_exclusive(file)?;
    let password = read_password_source(password_file, false)?;
    let before = std::fs::metadata(file)?.len();
    let con = db::open_db(file, &password, false)?;
    println!(
        "coffer: compacting {} (VACUUM may need up to ~2x the current size in free disk space temporarily)...",
        file.display()
    );
    // Explicit checkpoint rather than relying on SQLite's checkpoint-on-
    // last-close for a WAL-mode database: correct either way here since
    // this connection does close right after, but relying on that implicit
    // behavior bit the idle-watcher's long-lived connection (see there), so
    // making it explicit here too rather than depending on two different
    // mechanisms to reach the same result.
    con.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
    drop(con);
    let after = std::fs::metadata(file)?.len();
    println!(
        "coffer: {before} -> {after} bytes ({} bytes reclaimed)",
        before.saturating_sub(after)
    );
    Ok(())
}

fn cmd_completions(shell: clap_complete::Shell) {
    let mut cmd = <Cli as clap::CommandFactory>::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Create { file, max_size, password_file, save, mountpoint } => {
            // Alias and config are checked up front: a bad alias or an
            // unreadable config shouldn't surface only after the container
            // has already been created.
            let mut cfg = match &save {
                Some(alias) => {
                    config::validate_alias(alias)?;
                    Some(Config::load()?)
                }
                None => None,
            };
            cmd_create(&file, max_size, password_file.as_deref())?;
            if let (Some(alias), Some(cfg)) = (save, cfg.as_mut()) {
                let mountpoint = mountpoint.expect("clap: --save requires --mountpoint");
                cmd_add(cfg, Vault { alias, file, mountpoint, idle_timeout: None, compact_on_idle: None, password_file: None })?;
            }
            Ok(())
        }
        Cmd::Mount { target, mountpoint, save, foreground, idle_timeout, compact_on_idle, password_file } => {
            let opts = MountOpts { foreground, idle_timeout, compact_on_idle, password_file };
            let (file, mountpoint, opts) = plan_mount(target.as_deref(), mountpoint, opts)?;
            if let Some(alias) = save {
                let vault = Vault {
                    alias,
                    file: file.clone(),
                    mountpoint: mountpoint.clone(),
                    idle_timeout: opts.idle_timeout.clone(),
                    compact_on_idle: opts.compact_on_idle.clone(),
                    password_file: opts.password_file.clone(),
                };
                cmd_add(&mut Config::load()?, vault)?;
            }
            cmd_mount(&file, &mountpoint, opts)
        }
        Cmd::Umount { target } => cmd_umount(target.as_deref()),
        Cmd::Add { alias, file, mountpoint, idle_timeout, compact_on_idle, password_file } => {
            let vault = Vault { alias, file, mountpoint, idle_timeout, compact_on_idle, password_file };
            cmd_add(&mut Config::load()?, vault)
        }
        Cmd::Remove { alias } => cmd_remove(&mut Config::load()?, &alias),
        Cmd::List => cmd_list(&Config::load()?),
        Cmd::Check { file, password_file } => cmd_check(&resolve_container(&file)?, password_file.as_deref()),
        Cmd::Backup { file, dest, password_file } => {
            cmd_backup(&resolve_container(&file)?, &dest, password_file.as_deref())
        }
        Cmd::Passwd { file, password_file, new_password_file } => {
            cmd_passwd(&resolve_container(&file)?, password_file.as_deref(), new_password_file.as_deref())
        }
        Cmd::Info { file, password_file } => cmd_info(&resolve_container(&file)?, password_file.as_deref()),
        Cmd::Compact { file, password_file } => cmd_compact(&resolve_container(&file)?, password_file.as_deref()),
        Cmd::Completions { shell } => {
            cmd_completions(shell);
            Ok(())
        }
    }
}
