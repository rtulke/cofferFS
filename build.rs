fn main() {
    // Inside the release workflow's build containers `git rev-parse` fails
    // (the checkout is owned by a different uid than the one cargo runs
    // as, which git refuses as "dubious ownership"), so the packaged
    // binaries used to report "unknown". GitHub Actions exports the exact
    // commit as GITHUB_SHA; use that whenever git itself can't answer.
    let from_git = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let from_ci = std::env::var("GITHUB_SHA")
        .ok()
        .filter(|s| s.len() >= 7)
        .map(|s| s[..7].to_string());
    let hash = from_git.or(from_ci).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=COFFER_GIT_HASH={hash}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
}
