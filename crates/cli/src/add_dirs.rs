//! `--add-dir DIR`: more host directories the file tools may reach, on
//! top of the workspace root. Interactive and headless runs both build
//! their [`Workspace`] here, so neither can drop the added directories
//! when it rebuilds tools (a `/clear`, a model switch, a reload), and
//! subagents inherit them through the workspace they are handed.
//!
//! The root itself does not move: relative paths, the shell, the compute
//! kernels, memory, saved approvals, AGENTS.md and skills all stay keyed
//! to `--workspace`. Added directories contribute files, not instructions,
//! so several projects' AGENTS.md files never pile into one prompt.

use std::path::{Path, PathBuf};

use orca_harness_tools::Workspace;

use crate::Config;

/// One `--add-dir` value, checked while the flags are parsed so a typo
/// fails before a session file is created. Relative values resolve
/// against the current directory; the result is canonical.
pub(crate) fn parse(value: &str) -> Result<PathBuf, String> {
    let dir = Path::new(value)
        .canonicalize()
        .map_err(|error| format!("--add-dir {value}: {error}"))?;
    if !dir.is_dir() {
        return Err(format!("--add-dir {value}: not a directory"));
    }
    Ok(dir)
}

/// The run's workspace: the root plus every `--add-dir`. Fails when an
/// added directory does not exist or is not a directory, so a typo is
/// reported at startup rather than as refused tool calls later.
pub(crate) fn workspace(cfg: &Config) -> Result<Workspace, String> {
    build(&cfg.workspace, &cfg.add_dirs)
}

fn build(root: &Path, added: &[PathBuf]) -> Result<Workspace, String> {
    Workspace::new(root)
        .with_added_dirs(added.iter().cloned())
        .map_err(|error| format!("--add-dir: {error}"))
}

/// The system-prompt paragraph naming the added directories, or nothing
/// when there are none. Appended after the base prompt at the call site,
/// like user instructions, so `/clear` re-pushes it.
pub(crate) fn prompt_block(ws: &Workspace) -> Option<String> {
    let dirs = ws.added_dirs();
    if dirs.is_empty() {
        return None;
    }
    let list = dirs
        .iter()
        .map(|dir| format!("- {}", dir.display()))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "\n\nAdded directories. Besides the workspace you may read, search and edit \
         files in these directories:\n{list}\n\
         Address files in them by absolute path; relative paths still resolve against \
         the workspace root, and the shell starts there, so `cd` into an added \
         directory to run commands in it. Paths outside the workspace and these \
         directories are refused."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_args::parse_run_args;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orca-add-dirs-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn add_dir_repeats_in_order_for_interactive_and_headless_runs() {
        let a = temp("parse-a");
        let b = temp("parse-b");
        let (a_str, b_str) = (a.to_str().unwrap(), b.to_str().unwrap());
        let interactive = parse_run_args(args(&["--add-dir", a_str, "--add-dir", b_str])).unwrap();
        assert_eq!(interactive.add_dirs, vec![a.clone(), b.clone()]);
        let headless = parse_run_args(args(&[
            "--json",
            "-p",
            "hi",
            "--add-dir",
            a_str,
            "--add-dir",
            b_str,
        ]))
        .unwrap();
        assert_eq!(headless.add_dirs, vec![a, b]);
    }

    #[test]
    fn add_dir_requires_a_value() {
        let error = parse_run_args(args(&["--add-dir"])).err().unwrap();
        assert!(error.contains("--add-dir requires a value"), "{error}");
    }

    #[test]
    fn missing_or_non_directory_values_fail_while_parsing() {
        let root = temp("missing-root");
        let missing = root.join("does-not-exist");
        let error = parse_run_args(args(&["--add-dir", missing.to_str().unwrap()]))
            .err()
            .unwrap();
        assert!(error.starts_with("--add-dir "), "{error}");
        let file = root.join("file.txt");
        std::fs::write(&file, "x").unwrap();
        let error = parse(file.to_str().unwrap()).unwrap_err();
        assert!(error.ends_with("not a directory"), "{error}");
        // The workspace builder refuses a directory that vanished since.
        let error = build(&root, &[missing]).unwrap_err();
        assert!(error.starts_with("--add-dir: added directory"), "{error}");
    }

    #[test]
    fn workspace_canonicalizes_and_prompt_lists_added_directories() {
        let root = temp("prompt-root");
        let other = temp("prompt-other");
        let ws = build(&root, &[other.join("../orca-add-dirs-prompt-other")]).unwrap();
        assert_eq!(ws.added_dirs(), std::slice::from_ref(&other));
        let block = prompt_block(&ws).unwrap();
        assert!(block.contains(&format!("- {}", other.display())), "{block}");
        assert!(block.contains("by absolute path"));
        assert_eq!(prompt_block(&build(&root, &[]).unwrap()), None);
    }
}
