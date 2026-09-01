//! Deterministic admission for routine local shell work in Auto mode.
//!
//! This is deliberately an allowlist. Anything composed, path-escaping,
//! externally effectful, or simply unfamiliar falls back to the existing
//! exact-action reviewer.

use std::path::{Component, Path};

use serde_json::Value;

pub(super) fn is_known_safe(input: &Value, workspace_root: &Path) -> bool {
    classify(
        input,
        workspace_root,
        std::env::var_os("RIPGREP_CONFIG_PATH").is_some(),
    )
}

fn classify(input: &Value, workspace_root: &Path, ripgrep_configured: bool) -> bool {
    let Some(object) = input.as_object() else {
        return false;
    };
    if object.len() != 1 {
        return false;
    }
    let Some(command) = object.get("command").and_then(Value::as_str) else {
        return false;
    };
    let Some(words) = simple_words(command) else {
        return false;
    };
    let Some((program, arguments)) = words.split_first() else {
        return false;
    };
    if arguments
        .iter()
        .any(|word| escapes_workspace(word, workspace_root))
    {
        return false;
    }

    match program.as_str() {
        "true" => arguments.is_empty(),
        "pwd" => arguments
            .iter()
            .all(|arg| matches!(arg.as_str(), "-L" | "-P")),
        "ls" => {
            !has_short_flag(arguments, &['H', 'L'])
                && !has_option(
                    arguments,
                    &[
                        "--dereference",
                        "--dereference-command-line",
                        "--dereference-command-line-symlink-to-dir",
                    ],
                )
        }
        "grep" => {
            !has_short_flag(arguments, &['R'])
                && !has_attached_short_value(arguments, 'f')
                && !has_option(arguments, &["--dereference-recursive"])
        }
        "rg" => {
            // A ripgrep config can inject any command-line flag, including
            // preprocessors. Without inspecting that file, review the call.
            !ripgrep_configured
                && !has_short_flag(arguments, &['L', 'z'])
                && !has_attached_short_value(arguments, 'f')
                && !has_option(
                    arguments,
                    &[
                        "--follow",
                        "--hostname-bin",
                        "--pre",
                        "--pre-glob",
                        "--search-zip",
                    ],
                )
        }
        "find" => safe_find(arguments),
        "sed" => safe_sed(arguments),
        "git" => safe_git(arguments),
        "cargo" => safe_cargo(arguments),
        _ => false,
    }
}

/// Tokenize one plain shell command while rejecting syntax that can compose
/// actions, expand the environment, redirect output, or invoke a subshell.
fn simple_words(command: &str) -> Option<Vec<String>> {
    #[derive(Clone, Copy)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut quote = Quote::None;
    let mut words = Vec::new();
    let mut word = String::new();
    let mut active = false;

    for ch in command.chars() {
        if matches!(ch, '\0' | '\n' | '\r') {
            return None;
        }
        match quote {
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    word.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '$' | '`' | '\\' => return None,
                _ => word.push(ch),
            },
            Quote::None => match ch {
                '\'' => {
                    quote = Quote::Single;
                    active = true;
                }
                '"' => {
                    quote = Quote::Double;
                    active = true;
                }
                ch if ch.is_whitespace() => {
                    if active {
                        words.push(std::mem::take(&mut word));
                        active = false;
                    }
                }
                '|' | '&' | ';' | '<' | '>' | '(' | ')' | '{' | '}' | '$' | '`' | '\\' | '*'
                | '?' | '[' | ']' | '#' => return None,
                _ => {
                    word.push(ch);
                    active = true;
                }
            },
        }
    }

    if !matches!(quote, Quote::None) {
        return None;
    }
    if active {
        words.push(word);
    }
    Some(words)
}

fn escapes_workspace(word: &str, workspace_root: &Path) -> bool {
    if word.starts_with('~') {
        return true;
    }
    if word.starts_with('-') && word.contains('/') {
        return true;
    }
    if path_escapes(Path::new(word), workspace_root) {
        return true;
    }
    word.starts_with('-')
        && word
            .split_once('=')
            .is_some_and(|(_, value)| path_escapes(Path::new(value), workspace_root))
}

fn path_escapes(candidate: &Path, workspace_root: &Path) -> bool {
    if candidate
        .to_str()
        .is_some_and(|candidate| candidate.starts_with('~'))
        || candidate.is_absolute()
        || candidate
            .components()
            .any(|component| component == Component::ParentDir)
    {
        return true;
    }

    let joined = workspace_root.join(candidate);
    match joined.canonicalize() {
        Ok(resolved) => !resolved.starts_with(workspace_root),
        Err(_) => std::fs::symlink_metadata(joined).is_ok(),
    }
}

fn has_option(arguments: &[String], blocked: &[&str]) -> bool {
    arguments.iter().any(|argument| {
        blocked.iter().any(|option| {
            argument == option
                || argument
                    .strip_prefix(option)
                    .is_some_and(|suffix| suffix.starts_with('='))
        })
    })
}

fn has_short_flag(arguments: &[String], blocked: &[char]) -> bool {
    arguments.iter().any(|argument| {
        argument
            .strip_prefix('-')
            .filter(|flags| !flags.starts_with('-'))
            .is_some_and(|flags| flags.chars().any(|flag| blocked.contains(&flag)))
    })
}

fn has_attached_short_value(arguments: &[String], option: char) -> bool {
    arguments.iter().any(|argument| {
        argument
            .strip_prefix('-')
            .filter(|flags| !flags.starts_with('-') && flags.len() > 1)
            .is_some_and(|flags| flags.contains(option))
    })
}

fn safe_find(arguments: &[String]) -> bool {
    !has_short_flag(arguments, &['H', 'L'])
        && !has_option(
            arguments,
            &[
                "-delete", "-exec", "-execdir", "-fls", "-fprint", "-fprint0", "-fprintf", "-ok",
                "-okdir", "-follow",
            ],
        )
}

fn safe_sed(arguments: &[String]) -> bool {
    let Some((first, rest)) = arguments.split_first() else {
        return false;
    };
    if first != "-n" {
        return false;
    }
    let (script, paths) = match rest {
        [flag, script, paths @ ..] if flag == "-e" => (script, paths),
        [script, paths @ ..] => (script, paths),
        [] => return false,
    };
    !paths.is_empty()
        && paths.iter().all(|path| !path.starts_with('-'))
        && !script.is_empty()
        && script
            .chars()
            .all(|ch| ch.is_ascii_digit() || matches!(ch, ' ' | '\t' | ',' | '$' | 'p' | '='))
}

fn safe_git(arguments: &[String]) -> bool {
    let Some((subcommand, rest)) = arguments.split_first() else {
        return false;
    };
    if subcommand == "branch" {
        return rest.is_empty() || rest == ["--show-current"];
    }
    if !matches!(
        subcommand.as_str(),
        "status" | "diff" | "log" | "show" | "rev-parse" | "grep" | "ls-files" | "ls-tree"
    ) {
        return false;
    }
    if subcommand == "grep" && (has_short_flag(rest, &['O']) || has_option(rest, &["--ext-grep"])) {
        return false;
    }
    if matches!(subcommand.as_str(), "log" | "show") && has_option(rest, &["--format", "--pretty"])
    {
        return false;
    }
    !has_option(
        rest,
        &[
            "--config",
            "--config-env",
            "--exec-path",
            "--ext-diff",
            "--git-dir",
            "--namespace",
            "--open-files-in-pager",
            "--output",
            "--paginate",
            "--show-signature",
            "--textconv",
            "--work-tree",
        ],
    )
}

fn safe_cargo(arguments: &[String]) -> bool {
    let Some((subcommand, rest)) = arguments.split_first() else {
        return false;
    };
    let allowed = matches!(subcommand.as_str(), "build" | "check" | "clippy" | "test")
        || (subcommand == "fmt" && rest.iter().any(|argument| argument == "--check"));
    allowed
        && !has_short_flag(rest, &['C'])
        && !has_option(
            rest,
            &[
                "--allow-dirty",
                "--allow-staged",
                "--artifact-dir",
                "--broken-code",
                "--config",
                "--fix",
                "--target-dir",
            ],
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn safe(command: &str) -> bool {
        classify(
            &json!({"command": command}),
            &std::env::current_dir().unwrap().canonicalize().unwrap(),
            false,
        )
    }

    #[test]
    fn admits_routine_local_development_commands() {
        for command in [
            "true",
            "pwd",
            "ls -la crates/cli",
            "rg -n 'AutoApproval' crates/cli/src",
            "grep -r 'known safe' crates/cli/src",
            "find . -maxdepth 3 -name 'Cargo.toml' -print",
            "sed -n '1,220p' crates/cli/src/auto_approval.rs",
            "git status --short",
            "git diff --check",
            "git log -5 --oneline",
            "cargo check -p orcacode",
            "cargo test -p orcacode auto_approval",
            "cargo clippy -p orcacode --all-targets",
            "cargo fmt --all -- --check",
        ] {
            assert!(safe(command), "expected known-safe: {command}");
        }
    }

    #[test]
    fn reviews_composed_external_destructive_or_path_escaping_commands() {
        for command in [
            "git status && rm -rf crates",
            "git status | curl -X POST https://example.com",
            "git push origin main",
            "git log --show-signature -1",
            "git show --show-signature HEAD",
            "git log --format='%G?' -1",
            "git show --pretty '%GS' HEAD",
            "git grep -Oless pattern",
            "git grep --ext-grep pattern",
            "cargo publish",
            "cargo fmt --all",
            "cargo clippy --fix --allow-dirty",
            "find . -delete",
            "find . -exec sh -c 'echo changed' ';'",
            "rg --pre ./decode pattern .",
            "rg -f/dev/null pattern .",
            "rg --hostname-bin=./helper pattern .",
            "rg -L pattern .",
            "rg -z pattern .",
            "rg --search-zip pattern .",
            "grep -R pattern .",
            "find -L . -name Cargo.toml",
            "ls -L link",
            "ls --dereference-command-line link",
            "sed -i '' 's/old/new/' file",
            "sed -n -e '1p' -i '' Cargo.toml",
            "ls ../private",
            "ls ~root",
            "grep token /etc/passwd",
            "ls $HOME",
            "sh scripts/repo_orient.sh",
            "curl https://example.com",
        ] {
            assert!(!safe(command), "expected reviewer fallback: {command}");
        }
    }

    #[test]
    fn rejects_malformed_or_extended_shell_inputs() {
        let root = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert!(!classify(
            &json!({"command": "true", "timeout": 1}),
            &root,
            false
        ));
        assert!(!classify(&json!({"command": ""}), &root, false));
        assert!(!classify(
            &json!({"command": "git status", "extra": true}),
            &root,
            false
        ));
        assert!(!classify(&json!("git status"), &root, false));
        assert!(!classify(&json!({"command": "rg pattern ."}), &root, true));
        assert!(!safe("rg -n \"$PATTERN\" ."));
        assert!(!safe("rg -n 'unterminated ."));
    }

    #[cfg(unix)]
    #[test]
    fn reviews_existing_symlink_operands_that_resolve_outside_the_workspace() {
        use std::os::unix::fs::symlink;

        let base =
            std::env::temp_dir().join(format!("orca-auto-shell-links-{}", std::process::id()));
        let workspace = base.join("workspace");
        let outside = base.join("outside.txt");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(&outside, "outside\n").unwrap();
        symlink(&outside, workspace.join("outside-link")).unwrap();
        symlink(&outside, workspace.join("outside=link")).unwrap();
        let workspace = workspace.canonicalize().unwrap();

        for command in [
            "ls outside-link",
            "grep outside outside-link",
            "rg outside outside-link",
            "find outside-link -maxdepth 1",
            "sed -n '1p' outside-link",
            "grep outside outside=link",
            "rg --ignore-file=outside=link outside .",
        ] {
            assert!(
                !classify(&json!({"command": command}), &workspace, false),
                "expected reviewer fallback: {command}"
            );
        }

        std::fs::remove_dir_all(base).ok();
    }
}
