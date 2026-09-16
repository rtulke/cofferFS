fn main() {
    // Three sources, first one that answers wins:
    //  1. COFFER_GIT_HASH, set explicitly by builds from a release tarball
    //     (the AUR PKGBUILD and the RPM spec), where there is no .git at all.
    //  2. `git rev-parse` for ordinary checkouts.
    //  3. GITHUB_SHA inside the release workflow's build containers, where
    //     git refuses the checkout as "dubious ownership" (it is owned by a
    //     different uid than the one cargo runs as) - the packaged binaries
    //     used to report "unknown" because of that.
    let from_env = std::env::var("COFFER_GIT_HASH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
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
    let hash = from_env.or(from_git).or(from_ci).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=COFFER_GIT_HASH={hash}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-env-changed=COFFER_GIT_HASH");
}
