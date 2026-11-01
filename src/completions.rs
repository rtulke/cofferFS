//! Shell completions: clap_complete's scripts, plus the one thing they
//! can't know - the aliases registered in ~/.coffer/config. Wherever an
//! alias is accepted, bash, zsh and fish offer them, asked at <Tab> time
//! from the hidden `coffer __complete-aliases` (so $COFFER_CONFIG is
//! honoured), along with the files or directories that position also takes.

use clap_complete::Shell;

/// Subcommands whose first argument may be the alias of a registered
/// vault, and what the completion offers there besides the aliases.
const ALIAS_FIRST: &[(&str, &str)] = &[
    ("mount", "files"),
    ("check", "files"),
    ("backup", "files"),
    ("passwd", "files"),
    ("info", "files"),
    ("compact", "files"),
    ("umount", "dirs"),
    ("remove", ""),
];

pub fn generate(shell: Shell, cmd: &mut clap::Command) -> String {
    let name = cmd.get_name().to_string();
    let mut script = Vec::new();
    clap_complete::generate(shell, cmd, name, &mut script);
    let script = String::from_utf8_lossy(&script).into_owned();
    match shell {
        Shell::Bash => script + &bash(cmd),
        Shell::Zsh => zsh(&script),
        Shell::Fish => script + &fish(cmd),
        _ => script,
    }
}

/// Options that take a value, so that value isn't counted as an argument.
fn valued_options(cmd: &clap::Command) -> Vec<String> {
    let mut valued: Vec<String> = cmd
        .get_subcommands()
        .flat_map(|sub| sub.get_arguments())
        .filter(|a| !a.is_positional() && a.get_action().takes_values())
        .flat_map(|a| {
            let long = a.get_long().map(|l| format!("--{l}"));
            let short = a.get_short().map(|c| format!("-{c}"));
            long.into_iter().chain(short)
        })
        .collect();
    valued.sort();
    valued.dedup();
    valued
}

/// A wrapper around clap's _coffer: in the first argument of the
/// subcommands in ALIAS_FIRST it completes itself, anywhere else it hands
/// over.
fn bash(cmd: &clap::Command) -> String {
    let arms: String = ["files", "dirs", ""]
        .iter()
        .map(|kind| {
            let subs: Vec<&str> = ALIAS_FIRST.iter().filter(|(_, k)| k == kind).map(|(s, _)| *s).collect();
            format!("        {}) also={kind:?} ;;\n", subs.join("|"))
        })
        .collect();
    format!(
        r#"
# Vault aliases wherever one is accepted, on top of _coffer above
# (added by `coffer completions bash`).
_coffer_vaults() {{
    local cur="${{COMP_WORDS[COMP_CWORD]}}" sub="" pos=0 skip=0 i w also
    for (( i = 1; i < COMP_CWORD; i++ )); do
        w="${{COMP_WORDS[i]}}"
        if (( skip )); then
            # --opt=value arrives as three words: --opt, =, value
            [[ $w == "=" ]] || skip=0
            continue
        fi
        case "$w" in
            {valued}) skip=1 ;;
            -*) ;;
            *) if [[ -z $sub ]]; then sub=$w; else (( pos++ )); fi ;;
        esac
    done
    if (( skip || pos > 0 )) || [[ $cur == -* ]]; then
        _coffer "$@"
        return
    fi
    case "$sub" in
{arms}        *) _coffer "$@"; return ;;
    esac
    # compgen rather than bash-completion's _filedir, which completes an
    # empty word to nothing - bash's own file fallback would make up for
    # that, but it only steps in when there are no aliases either.
    local IFS=$'\n'
    local aliases=($(compgen -W "$("$1" __complete-aliases 2>/dev/null)" -- "$cur")) files=()
    case "$also" in
        files) files=($(compgen -f -- "$cur")) ;;
        dirs) files=($(compgen -d -- "$cur")) ;;
    esac
    # Quote what needs it and give directories their slash.
    (( ${{#files[@]}} )) && compopt -o filenames 2>/dev/null
    COMPREPLY=("${{aliases[@]}}" "${{files[@]}}")
}}

if [[ "${{BASH_VERSINFO[0]}}" -eq 4 && "${{BASH_VERSINFO[1]}}" -ge 4 || "${{BASH_VERSINFO[0]}}" -gt 4 ]]; then
    complete -F _coffer_vaults -o nosort -o bashdefault -o default coffer
else
    complete -F _coffer_vaults -o bashdefault -o default coffer
fi
"#,
        valued = valued_options(cmd).join("|"),
    )
}

/// Defined ahead of the line that runs _coffer on the first <Tab> (the
/// whole file is the body of the autoloaded function), so it exists by
/// the time an action names it.
const ZSH_HELPER: &str = r#"(( $+functions[_coffer__vaults] )) ||
_coffer__vaults() {
    local -a aliases alts
    aliases=(${(f)"$(_call_program vaults $words[1] __complete-aliases 2>/dev/null)"})
    alts=('vaults:registered vault:compadd -a aliases')
    case $1 in
        files) alts+=('files:container file:_files') ;;
        dirs) alts+=('directories:mountpoint:_files -/') ;;
    esac
    _alternative $alts
}

"#;

/// clap writes one `_arguments` spec per argument, the action last
/// (`':file -- help text:_default' \`). In the block of each subcommand in
/// ALIAS_FIRST, the first positional spec gets `_coffer__vaults KIND` as
/// its action instead.
fn zsh(script: &str) -> String {
    let mut out = String::with_capacity(script.len() + ZSH_HELPER.len() + 256);
    let mut helper_placed = false;
    // The kind of the subcommand whose block we are in, until its first positional.
    let mut pending: Option<&str> = None;
    for line in script.lines() {
        let trimmed = line.trim();
        if let Some(label) = trimmed.strip_prefix('(').and_then(|l| l.strip_suffix(')')) {
            pending = ALIAS_FIRST.iter().find(|(sub, _)| *sub == label).map(|(_, kind)| *kind);
        }
        if !helper_placed && trimmed.starts_with(r#"if [ "$funcstack[1]" = "_coffer" ]"#) {
            out.push_str(ZSH_HELPER);
            helper_placed = true;
        }
        match pending {
            Some(kind) if line.starts_with("':") => {
                // Colons in the help text are escaped, the action has none:
                // it is what follows the last colon, up to the closing quote.
                let action = line.rfind(':').and_then(|c| line[c..].find('\'').map(|q| c + 1..c + q));
                match action {
                    Some(range) => {
                        out.push_str(&line[..range.start]);
                        out.push_str(format!("_coffer__vaults {kind}").trim_end());
                        out.push_str(&line[range.end..]);
                    }
                    None => out.push_str(line),
                }
                pending = None;
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    if !helper_placed {
        out.push_str(ZSH_HELPER);
    }
    out
}

/// fish asks every rule's condition anyway, so this only adds rules: the
/// aliases while the subcommand has no argument yet. Files stay on offer
/// there, except for `remove`, which takes nothing but an alias.
fn fish(cmd: &clap::Command) -> String {
    let with_files: Vec<&str> = ALIAS_FIRST.iter().filter(|(_, k)| !k.is_empty()).map(|(s, _)| *s).collect();
    let alias_only: Vec<&str> = ALIAS_FIRST.iter().filter(|(_, k)| k.is_empty()).map(|(s, _)| *s).collect();
    format!(
        r#"
# Vault aliases wherever one is accepted (added by `coffer completions fish`).
function __fish_coffer_no_argument_yet
    set -l tokens (commandline -opc)
    set -e tokens[1..2]
    set -l skip 0
    for t in $tokens
        if test $skip = 1
            set skip 0
            continue
        end
        switch $t
            case {valued}
                set skip 1
            case '-*'
            case '*'
                return 1
        end
    end
    test $skip = 0
end

complete -c coffer -n "__fish_coffer_using_subcommand {with_files}; and __fish_coffer_no_argument_yet" -a "(coffer __complete-aliases 2>/dev/null)" -d 'registered vault'
complete -c coffer -n "__fish_coffer_using_subcommand {alias_only}; and __fish_coffer_no_argument_yet" -f -a "(coffer __complete-aliases 2>/dev/null)" -d 'registered vault'
"#,
        valued = valued_options(cmd).join(" "),
        with_files = with_files.join(" "),
        alias_only = alias_only.join(" "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli() -> clap::Command {
        <crate::Cli as clap::CommandFactory>::command()
    }

    #[test]
    fn alias_positions_exist() {
        let cmd = cli();
        for (name, _) in ALIAS_FIRST {
            let sub = cmd.find_subcommand(name).unwrap_or_else(|| panic!("no subcommand {name}"));
            assert!(sub.get_positionals().next().is_some(), "{name} has no argument to complete");
        }
    }

    /// The shells skip an option's value by the option's name alone, so a
    /// name must not take a value in one subcommand and none in another.
    #[test]
    fn option_names_take_a_value_everywhere_or_nowhere() {
        let cmd = cli();
        let valued = valued_options(&cmd);
        for sub in cmd.get_subcommands() {
            for a in sub.get_arguments().filter(|a| !a.is_positional() && !a.get_action().takes_values()) {
                let names = a.get_long().map(|l| format!("--{l}")).into_iter().chain(a.get_short().map(|c| format!("-{c}")));
                for n in names {
                    assert!(!valued.contains(&n), "{n} is a plain flag in {} but takes a value elsewhere", sub.get_name());
                }
            }
        }
    }

    #[test]
    fn zsh_actions_replaced() {
        let script = generate(Shell::Zsh, &mut cli());
        let replaced: Vec<&str> = script.lines().filter(|l| l.starts_with("':") && l.contains(":_coffer__vaults")).collect();
        assert_eq!(replaced.len(), ALIAS_FIRST.len(), "{replaced:#?}");
        assert!(script.contains("':alias:_coffer__vaults' \\"), "remove: aliases only");
        assert!(script.contains(":_coffer__vaults dirs' \\"), "umount: directories");
        let helper = script.find("_coffer__vaults() {").unwrap();
        let tail = script.find(r#"if [ "$funcstack[1]" = "_coffer" ]"#).unwrap();
        assert!(helper < tail, "the helper has to exist before the first _coffer call");
        // `add` names a new alias - nothing to complete from the registry.
        let add = &script[script.find("\n(add)\n").unwrap()..];
        let add = &add[..add.find(";;").unwrap()];
        assert!(!add.contains("_coffer__vaults"), "{add}");
    }

    #[test]
    fn fish_builds_on_clap() {
        let script = generate(Shell::Fish, &mut cli());
        assert!(script.contains("function __fish_coffer_using_subcommand"), "clap renamed its helper");
        assert!(script.contains("__fish_coffer_using_subcommand mount check backup passwd info compact umount; and"));
        assert!(script.contains("__fish_coffer_using_subcommand remove; and __fish_coffer_no_argument_yet\" -f"));
    }

    #[test]
    fn bash_syntax() {
        let script = generate(Shell::Bash, &mut cli());
        let Ok(mut child) = std::process::Command::new("bash")
            .arg("-n")
            .stdin(std::process::Stdio::piped())
            .spawn()
        else {
            return; // no bash here
        };
        use std::io::Write;
        child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
    }
}
