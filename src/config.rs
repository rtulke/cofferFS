//! The per-user vault registry: `~/.coffer/config` (or `$COFFER_CONFIG`).
//!
//! A tiny INI-style file, one section per registered vault, the section
//! name being the alias `coffer mount <alias>` and friends look up:
//!
//! ```text
//! [work]
//! file = /home/me/.coffer/work.coffer
//! mountpoint = /home/me/vault
//! idle_timeout = 30m          # optional, same format as --idle-timeout
//! compact_on_idle = 2h        # optional, same format as --compact-on-idle
//! password_file = ~/.coffer/work.pw   # optional, same as --password-file
//! password_command = pass show coffer/work   # optional, same as --password-command
//! log_file = ~/.coffer/work.log      # optional, same as --log
//! read_only = true                    # optional, same as --read-only
//! ```
//!
//! Parsed by hand rather than through a TOML/serde dependency: the format
//! is deliberately this small, and keeping it line-oriented is what lets
//! `coffer add` / `coffer remove` rewrite one section while leaving every
//! other line - hand-written comments included - exactly as it was.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

const DEFAULT_HEADER: &str = "\
# coffer vault registry - maintained by `coffer add` / `coffer remove`,
# shown by `coffer list`. One section per vault; the section name is the
# alias for `coffer mount <alias>`. Safe to edit by hand: comments on their
# own line are kept, `~` at the start of a path means $HOME.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vault {
    pub alias: String,
    pub file: PathBuf,
    pub mountpoint: PathBuf,
    pub idle_timeout: Option<String>,
    pub compact_on_idle: Option<String>,
    pub password_file: Option<PathBuf>,
    pub password_command: Option<String>,
    pub log_file: Option<PathBuf>,
    pub read_only: bool,
}

struct Section {
    name: String,
    /// Raw lines *after* the `[name]` header - comments, blanks and all.
    lines: Vec<String>,
}

pub struct Config {
    pub path: PathBuf,
    preamble: Vec<String>,
    sections: Vec<Section>,
}

pub fn config_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("COFFER_CONFIG") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    Ok(home_dir()?.join(".coffer").join("config"))
}

fn home_dir() -> Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => Ok(PathBuf::from(h)),
        _ => bail!("$HOME is not set, so ~/.coffer/config cannot be located (set $COFFER_CONFIG to point at it explicitly)"),
    }
}

/// `~` and `~/...` mean $HOME, as they would in a shell; everything else is
/// taken literally. Only the leading form - `~user/...` is not supported.
pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') {
            if let Ok(home) = home_dir() {
                return home.join(rest.trim_start_matches('/'));
            }
        }
    }
    PathBuf::from(s)
}

/// The reverse, for display only: `/home/me/x` -> `~/x`.
pub fn abbreviate_home(path: &Path) -> String {
    if let Ok(home) = home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

/// Aliases double as section names in the file and as bare-word positionals
/// on the command line, so they're kept to a shape that can never be
/// mistaken for a path (no `/`, no leading `.`) or for a flag (no leading `-`).
pub fn validate_alias(alias: &str) -> Result<()> {
    let ok_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    let starts_ok = alias.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
    if !starts_ok || !alias.chars().all(ok_char) {
        bail!(
            "invalid alias {alias:?}: must start with a letter or digit and contain only letters, \
digits, '-', '_' and '.'"
        );
    }
    Ok(())
}

fn unquote(v: &str) -> &str {
    let v = v.trim();
    if v.len() >= 2 && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\''))) {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.is_empty() || t.starts_with('#') || t.starts_with(';')
}

impl Config {
    pub fn load() -> Result<Config> {
        Config::load_from(config_path()?)
    }

    pub fn load_from(path: PathBuf) -> Result<Config> {
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(path, &text),
            // A never-created config is simply empty; the header only gets
            // written once the first vault is registered.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config {
                path,
                preamble: DEFAULT_HEADER.lines().map(String::from).chain([String::new()]).collect(),
                sections: Vec::new(),
            }),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn parse(path: PathBuf, text: &str) -> Result<Config> {
        let mut preamble = Vec::new();
        let mut sections: Vec<Section> = Vec::new();
        for (idx, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.starts_with('[') {
                let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) else {
                    bail!("{}:{}: malformed section header {raw:?}", path.display(), idx + 1);
                };
                let name = name.trim();
                validate_alias(name).with_context(|| format!("{}:{}", path.display(), idx + 1))?;
                if sections.iter().any(|s| s.name == name) {
                    bail!("{}:{}: duplicate section [{name}]", path.display(), idx + 1);
                }
                sections.push(Section { name: name.to_string(), lines: Vec::new() });
            } else if let Some(section) = sections.last_mut() {
                section.lines.push(raw.to_string());
            } else {
                preamble.push(raw.to_string());
            }
        }
        Ok(Config { path, preamble, sections })
    }

    pub fn vaults(&self) -> Result<Vec<Vault>> {
        self.sections.iter().map(|s| self.vault_from(s)).collect()
    }

    pub fn get(&self, alias: &str) -> Result<Option<Vault>> {
        self.sections.iter().find(|s| s.name == alias).map(|s| self.vault_from(s)).transpose()
    }

    fn vault_from(&self, section: &Section) -> Result<Vault> {
        let where_ = |what: &str| format!("{}: [{}]: {what}", self.path.display(), section.name);
        let (mut file, mut mountpoint, mut idle_timeout, mut compact_on_idle, mut password_file, mut log_file) =
            (None, None, None, None, None, None);
        let mut password_command = None;
        let mut read_only = false;
        for raw in &section.lines {
            if is_comment(raw) {
                continue;
            }
            let Some((key, value)) = raw.split_once('=') else {
                bail!(where_(&format!("expected `key = value`, got {raw:?}")));
            };
            let value = unquote(value);
            if value.is_empty() {
                bail!(where_(&format!("`{}` has no value", key.trim())));
            }
            match key.trim() {
                "file" => file = Some(expand_tilde(value)),
                "mountpoint" => mountpoint = Some(expand_tilde(value)),
                "idle_timeout" => idle_timeout = Some(value.to_string()),
                "compact_on_idle" => compact_on_idle = Some(value.to_string()),
                "password_file" => password_file = Some(expand_tilde(value)),
                "password_command" => password_command = Some(value.to_string()),
                "log_file" => log_file = Some(expand_tilde(value)),
                "read_only" => {
                    read_only = match value.to_ascii_lowercase().as_str() {
                        "true" | "yes" | "on" | "1" => true,
                        "false" | "no" | "off" | "0" => false,
                        _ => bail!(where_(&format!("`read_only` must be true or false, got {value:?}"))),
                    }
                }
                other => bail!(where_(&format!(
                    "unknown key {other:?} (expected file, mountpoint, idle_timeout, compact_on_idle, password_file, password_command, log_file or read_only)"
                ))),
            }
        }
        Ok(Vault {
            alias: section.name.clone(),
            file: file.ok_or_else(|| anyhow!(where_("missing `file = ...`")))?,
            mountpoint: mountpoint.ok_or_else(|| anyhow!(where_("missing `mountpoint = ...`")))?,
            idle_timeout,
            compact_on_idle,
            password_file,
            password_command,
            log_file,
            read_only,
        })
    }

    /// Adds `vault`, or replaces the section of the same alias. Returns
    /// whether it replaced one.
    pub fn upsert(&mut self, vault: &Vault) -> bool {
        let lines = render(vault);
        if let Some(section) = self.sections.iter_mut().find(|s| s.name == vault.alias) {
            // Keep the blank line that separated it from the next section.
            let trailing_blank = section.lines.last().is_some_and(|l| l.trim().is_empty());
            section.lines = lines;
            if trailing_blank {
                section.lines.push(String::new());
            }
            return true;
        }
        // Appended, with a blank line between it and whatever came before.
        let prev = self.sections.last_mut().map(|s| &mut s.lines).unwrap_or(&mut self.preamble);
        if prev.last().is_some_and(|l| !l.trim().is_empty()) {
            prev.push(String::new());
        }
        self.sections.push(Section { name: vault.alias.clone(), lines });
        false
    }

    /// Returns whether there was a section to remove.
    pub fn remove(&mut self, alias: &str) -> bool {
        let before = self.sections.len();
        self.sections.retain(|s| s.name != alias);
        self.sections.len() != before
    }

    fn to_text(&self) -> String {
        let mut out = String::new();
        for line in &self.preamble {
            out.push_str(line);
            out.push('\n');
        }
        for section in &self.sections {
            out.push_str(&format!("[{}]\n", section.name));
            for line in &section.lines {
                out.push_str(line);
                out.push('\n');
            }
        }
        // Don't let a run of separator blanks grow by one on every rewrite.
        while out.ends_with("\n\n") {
            out.pop();
        }
        out
    }

    /// Written via a temp file + rename, so a crash mid-write can never leave
    /// a half-written registry behind; mode 0600, and a freshly created
    /// ~/.coffer directory gets 0700 (the file can name password files).
    pub fn save(&self) -> Result<()> {
        use std::io::Write;
        let dir = match self.path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        if !dir.exists() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(self.to_text().as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &self.path).with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}

fn render(vault: &Vault) -> Vec<String> {
    let mut lines = vec![
        format!("file = {}", vault.file.display()),
        format!("mountpoint = {}", vault.mountpoint.display()),
    ];
    if let Some(v) = &vault.idle_timeout {
        lines.push(format!("idle_timeout = {v}"));
    }
    if let Some(v) = &vault.compact_on_idle {
        lines.push(format!("compact_on_idle = {v}"));
    }
    if let Some(v) = &vault.password_file {
        lines.push(format!("password_file = {}", v.display()));
    }
    if let Some(v) = &vault.password_command {
        lines.push(format!("password_command = {v}"));
    }
    if let Some(v) = &vault.log_file {
        lines.push(format!("log_file = {}", v.display()));
    }
    if vault.read_only {
        lines.push("read_only = true".to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> Config {
        Config::parse(PathBuf::from("/test/config"), text).unwrap()
    }

    fn vault(alias: &str) -> Vault {
        Vault {
            alias: alias.into(),
            file: PathBuf::from(format!("/data/{alias}.coffer")),
            mountpoint: PathBuf::from(format!("/mnt/{alias}")),
            idle_timeout: None,
            compact_on_idle: None,
            password_file: None,
            password_command: None,
            log_file: None,
            read_only: false,
        }
    }

    #[test]
    fn parses_sections_and_options() {
        let c = cfg("# header\n\n[work]\nfile = /a.coffer\nmountpoint = \"/mnt/a\"\nidle_timeout = 30m\n\n[photos]\n; comment\nfile=/b.coffer\nmountpoint=/mnt/b\ncompact_on_idle = 2h\npassword_file = /pw\n");
        let v = c.vaults().unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].alias, "work");
        assert_eq!(v[0].file, PathBuf::from("/a.coffer"));
        assert_eq!(v[0].mountpoint, PathBuf::from("/mnt/a"));
        assert_eq!(v[0].idle_timeout.as_deref(), Some("30m"));
        assert_eq!(v[0].compact_on_idle, None);
        assert_eq!(v[1].alias, "photos");
        assert_eq!(v[1].compact_on_idle.as_deref(), Some("2h"));
        assert_eq!(v[1].password_file, Some(PathBuf::from("/pw")));
        assert_eq!(c.get("photos").unwrap().unwrap().alias, "photos");
        assert!(c.get("nope").unwrap().is_none());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Config::parse("/c".into(), "[a\nfile=/x\n").is_err());
        assert!(Config::parse("/c".into(), "[a]\n[a]\n").is_err());
        assert!(Config::parse("/c".into(), "[bad/name]\n").is_err());
        assert!(cfg("[a]\nfile=/x\n").vaults().is_err(), "missing mountpoint");
        assert!(cfg("[a]\nfile=/x\nmountpoint=/m\nbogus=1\n").vaults().is_err(), "unknown key");
        assert!(cfg("[a]\nfile=/x\nmountpoint=/m\njunk\n").vaults().is_err(), "line without =");
        assert!(cfg("[a]\nfile=\nmountpoint=/m\n").vaults().is_err(), "empty value");
        assert!(cfg("[a]\nfile=/x\nmountpoint=/m\nread_only=maybe\n").vaults().is_err(), "bad read_only");
        assert!(cfg("[a]\nfile=/x\nmountpoint=/m\nread_only=yes\n").vaults().unwrap()[0].read_only);
        for bad in ["", "-x", ".x", "a b", "a/b", "a[b]"] {
            assert!(validate_alias(bad).is_err(), "{bad:?} should be rejected");
        }
        for good in ["a", "work", "my-vault_2.old", "1"] {
            assert!(validate_alias(good).is_ok(), "{good:?} should be accepted");
        }
    }

    #[test]
    fn upsert_and_remove_keep_other_lines_intact() {
        let mut c = cfg("# my notes\n[work]\n# keep me\nfile = /a.coffer\nmountpoint = /mnt/a\n\n[photos]\nfile = /b.coffer\nmountpoint = /mnt/b");
        assert!(c.upsert(&vault("work")), "replacing an existing alias");
        assert!(!c.upsert(&vault("new")), "adding a new alias");
        assert_eq!(
            c.to_text(),
            "# my notes\n[work]\nfile = /data/work.coffer\nmountpoint = /mnt/work\n\n[photos]\nfile = /b.coffer\nmountpoint = /mnt/b\n\n[new]\nfile = /data/new.coffer\nmountpoint = /mnt/new\n"
        );
        assert!(c.remove("photos"));
        assert!(!c.remove("photos"));
        assert_eq!(
            c.to_text(),
            "# my notes\n[work]\nfile = /data/work.coffer\nmountpoint = /mnt/work\n\n[new]\nfile = /data/new.coffer\nmountpoint = /mnt/new\n"
        );
        // Round-trips through the parser unchanged.
        assert_eq!(cfg(&c.to_text()).to_text(), c.to_text());
    }

    #[test]
    fn fresh_config_gets_header_and_renders_options() {
        let mut c = Config::load_from(PathBuf::from("/nonexistent/dir/config")).unwrap();
        assert!(c.vaults().unwrap().is_empty());
        let mut v = vault("work");
        v.idle_timeout = Some("30m".into());
        v.compact_on_idle = Some("2h".into());
        v.password_file = Some("/pw".into());
        v.password_command = Some("pass show x = y".into());
        v.log_file = Some("/log".into());
        v.read_only = true;
        c.upsert(&v);
        let text = c.to_text();
        assert!(text.starts_with("# coffer vault registry"));
        assert!(text.ends_with("\n[work]\nfile = /data/work.coffer\nmountpoint = /mnt/work\nidle_timeout = 30m\ncompact_on_idle = 2h\npassword_file = /pw\npassword_command = pass show x = y\nlog_file = /log\nread_only = true\n"));
        assert_eq!(cfg(&text).vaults().unwrap(), vec![v]);
    }

    #[test]
    fn tilde_expansion() {
        let home = std::env::var("HOME").expect("HOME set in tests");
        assert_eq!(expand_tilde("~/x/y"), PathBuf::from(format!("{home}/x/y")));
        assert_eq!(expand_tilde("~"), PathBuf::from(&home));
        assert_eq!(expand_tilde("~other/x"), PathBuf::from("~other/x"));
        assert_eq!(expand_tilde("/abs/~/x"), PathBuf::from("/abs/~/x"));
        assert_eq!(abbreviate_home(Path::new(&format!("{home}/v"))), "~/v");
        assert_eq!(abbreviate_home(Path::new("/elsewhere")), "/elsewhere");
    }
}
