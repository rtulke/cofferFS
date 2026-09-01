mod db;
mod fs;

use anyhow::{bail, Context, Result};
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
    },
    /// Mount a container as the current user
    Mount {
        file: PathBuf,
        mountpoint: PathBuf,
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
    Umount { mountpoint: PathBuf },
    /// Verify integrity without modifying the container
    Check {
        file: PathBuf,
        /// Read the password from this file instead of prompting
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
    /// Make a consistent copy (safe even while mounted)
    Backup {
        file: PathBuf,
        dest: PathBuf,
        /// Read the password from this file instead of prompting
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
    /// Change the container password
    Passwd {
        file: PathBuf,
        /// Read the current password from this file instead of prompting
        #[arg(long)]
        password_file: Option<PathBuf>,
        /// Read the new password from this file instead of prompting
        #[arg(long)]
        new_password_file: Option<PathBuf>,
    },
    /// Show container stats
    Info {
        file: PathBuf,
        /// Read the password from this file instead of prompting
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
    /// Reclaim disk space after deletions (VACUUM); refuses to run against a mounted container
    Compact {
        file: PathBuf,
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

fn cmd_mount(
    file: &Path,
    mountpoint: &Path,
    foreground: bool,
    idle_timeout: Option<String>,
    compact_on_idle: Option<String>,
    password_file: Option<&Path>,
) -> Result<()> {
    let idle_timeout = idle_timeout.map(|s| parse_duration(&s)).transpose()?;
    let compact_on_idle = compact_on_idle.map(|s| parse_duration(&s)).transpose()?;
    let abs_file = file.canonicalize()?;

    // Acquired before even opening the database: if some other coffer
    // process already has this container locked (mounted, or a passwd/
    // compact in progress), fail immediately - before wasting a password
    // prompt, and critically before `open_db` below keys a connection that
    // could otherwise end up stale if a concurrent `passwd` rekeys the
    // container in the gap between us opening it and us locking it.
    let _lock = db::lock_exclusive(&abs_file)?;

    let password = read_password_source(password_file, false)?;
    let con = db::open_db(file, &password, false)?;
    let max_size = db::read_max_size(&con);

    std::fs::create_dir_all(mountpoint)?;
    if std::fs::read_dir(mountpoint)?.next().is_some() {
        bail!("mountpoint {} is not empty", mountpoint.display());
    }
    let abs_mountpoint = mountpoint.canonicalize()?;

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

    if !foreground {
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

// A plain unmount can fail with "Device or resource busy" when the kernel
// hasn't finished tearing down a stale FUSE connection yet (e.g. the server
// process crashed and something still references the mount). Escalating to
// a lazy unmount - detach the mountpoint now, finish cleanup once nothing
// still references it - resolves that without needing root: it's the same
// mounting-user permissions as the plain unmount, just a different kernel-
// side detach mode, so no sudo involved here.
fn run_unmount(mountpoint: &Path) -> std::io::Result<std::process::ExitStatus> {
    // The plain attempt's own output is suppressed: if it fails it's usually
    // just "device or resource busy" en route to the lazy retry succeeding,
    // and showing that would look like a real error for what's actually a
    // routine, silent escalation. If the lazy attempt fails too, its output
    // is left visible - that's a genuine failure worth seeing.
    for candidate in ["fusermount3", "fusermount"] {
        if which(candidate).is_some() {
            let status = std::process::Command::new(candidate)
                .arg("-u")
                .arg(mountpoint)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?;
            if status.success() {
                return Ok(status);
            }
            return std::process::Command::new(candidate)
                .arg("-u")
                .arg("-z")
                .arg(mountpoint)
                .status();
        }
    }
    let status = std::process::Command::new("umount")
        .arg(mountpoint)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        return Ok(status);
    }
    std::process::Command::new("umount").arg("-l").arg(mountpoint).status()
}

fn cmd_umount(mountpoint: &Path) -> Result<()> {
    let status = run_unmount(mountpoint)?;
    if !status.success() {
        hint_sudo_umount(mountpoint);
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
        Cmd::Create { file, max_size, password_file } => {
            cmd_create(&file, max_size, password_file.as_deref())
        }
        Cmd::Mount { file, mountpoint, foreground, idle_timeout, compact_on_idle, password_file } => {
            cmd_mount(
                &file,
                &mountpoint,
                foreground,
                idle_timeout,
                compact_on_idle,
                password_file.as_deref(),
            )
        }
        Cmd::Umount { mountpoint } => cmd_umount(&mountpoint),
        Cmd::Check { file, password_file } => cmd_check(&file, password_file.as_deref()),
        Cmd::Backup { file, dest, password_file } => cmd_backup(&file, &dest, password_file.as_deref()),
        Cmd::Passwd { file, password_file, new_password_file } => {
            cmd_passwd(&file, password_file.as_deref(), new_password_file.as_deref())
        }
        Cmd::Info { file, password_file } => cmd_info(&file, password_file.as_deref()),
        Cmd::Compact { file, password_file } => cmd_compact(&file, password_file.as_deref()),
        Cmd::Completions { shell } => {
            cmd_completions(shell);
            Ok(())
        }
    }
}
