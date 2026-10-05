//! The shell behind `DockerSandbox::protect`.
//!
//! Protection can run on a live container (`finalize_named`), where agent
//! processes may rename entries they own while root works. Every change is
//! therefore made from inside the directory: `cd -P` pins the shell to one
//! directory, `.` keeps naming it however the path is renamed or relinked
//! afterwards, and the command refuses unless that directory sits at the
//! expected path both before and after the change. A symlink swapped in at
//! any point can only make the command fail, never carry a chown or chmod
//! out of place.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use orca_harness_core::SandboxError;

use super::parse::shell_quote;

/// Make an ancestor of a capability tree root-owned and sticky.
pub(super) const ANCESTOR: &str = "chown 0:0 . && chmod 1777 .";

/// Make a capability tree root-owned and read-only, refusing one that
/// holds a symlink.
pub(super) const TREE: &str =
    "test -z \"$(find . -type l -print -quit)\" && chown -hR 0:0 . && chmod -R a-w .";

/// `directory` as an absolute path of plain components: no `..`, `.`,
/// repeated or trailing slash, and not `/` itself.
pub(super) fn canonical(directory: &str) -> Result<String, SandboxError> {
    let path = Path::new(directory);
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_string_lossy()),
            Component::ParentDir | Component::Prefix(_) => return Err(refuse(directory)),
        }
    }
    if !path.is_absolute() || parts.is_empty() {
        return Err(refuse(directory));
    }
    Ok(format!("/{}", parts.join("/")))
}

fn refuse(directory: &str) -> SandboxError {
    SandboxError::Provision(format!(
        "protected directory must be absolute, below / and without parent traversal: {directory}"
    ))
}

/// The directories strictly between `workspace` and each of `directories`,
/// all canonical. Sorted, so a parent always comes before its children.
pub(super) fn ancestors(workspace: &str, directories: &[String]) -> BTreeSet<String> {
    let inside = format!("{workspace}/");
    let mut ancestors = BTreeSet::new();
    for directory in directories {
        let mut path = Path::new(directory).parent();
        while let Some(parent) = path {
            let text = parent.to_string_lossy();
            if !text.starts_with(&inside) {
                break;
            }
            ancestors.insert(text.into_owned());
            path = parent.parent();
        }
    }
    ancestors
}

/// One shell command that creates `directory` (canonical) if needed, enters
/// it and runs `action` there, refusing unless the directory entered is the
/// one at `directory` before and after `action`. The parent must already be
/// out of the agent's reach, which [`ancestors`] order guarantees.
pub(super) fn anchored(directory: &str, action: &str) -> String {
    let path = Path::new(directory);
    let parent = path.parent().map_or("/".into(), |p| p.to_string_lossy());
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let here = format!("\"${{parent%/}}/\"{}", shell_quote(&name));
    let refusal = format!("refusing to protect {directory}: it is not a directory in place");
    format!(
        "mkdir -p -- {dir} && parent=\"$(cd -P -- {parent} && pwd -P)\" && cd -P -- {dir} \
         && [ \"$(pwd -P)\" = {here} ] && {action} && [ \"$(pwd -P)\" = {here} ] \
         || {{ echo {refusal} >&2; exit 1; }}",
        dir = shell_quote(directory),
        parent = shell_quote(&parent),
        refusal = shell_quote(&refusal),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_are_canonical_or_refused() {
        assert_eq!(canonical("/workspace").unwrap(), "/workspace");
        assert_eq!(canonical("/workspace/./a//b/").unwrap(), "/workspace/a/b");
        for refused in ["/", "//", "/.", "relative", "", "/workspace/../etc"] {
            assert!(canonical(refused).is_err(), "{refused:?} was accepted");
        }
    }

    #[test]
    fn ancestors_stop_at_the_workspace_and_sort_parents_first() {
        let found = ancestors(
            "/workspace",
            &["/workspace/.orca/skills/a".into(), "/opt/tools".into()],
        );
        let found: Vec<_> = found.into_iter().collect();
        assert_eq!(found, ["/workspace/.orca", "/workspace/.orca/skills"]);
    }

    fn sh(command: &str) -> std::process::Output {
        std::process::Command::new("sh")
            .args(["-c", command])
            .output()
            .unwrap()
    }

    /// The command itself, run with a harmless action: it acts inside a
    /// real directory and refuses a symlink, whatever it points at.
    #[test]
    fn anchored_commands_act_in_place_and_refuse_symlinks() {
        let base = std::env::temp_dir().join(format!("orca-protect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let root = canonical(&base.join("root").to_string_lossy()).unwrap();
        std::fs::create_dir_all(&root).unwrap();

        let made = format!("{root}/made");
        let output = sh(&anchored(&made, "touch ./marker"));
        assert!(output.status.success(), "{output:?}");
        assert!(Path::new(&made).join("marker").exists());

        let link = format!("{root}/link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let output = sh(&anchored(&link, "touch ./marker"));
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains(&format!("refusing to protect {link}")));
        assert!(!outside.join("marker").exists());

        // An action that moves the directory out of place is refused too.
        let moved = format!("{root}/moved");
        let output = sh(&anchored(&moved, &format!("mv -- {moved} {root}/away")));
        assert!(!output.status.success());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
