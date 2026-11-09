//! The directories a [`Workspace`](super::Workspace) may touch: its main
//! root plus any directories added with
//! [`Workspace::with_added_dirs`](super::Workspace::with_added_dirs).
//!
//! Every path a file tool receives is resolved here, and one rule decides
//! whether it is allowed: after `..` is folded away and every existing
//! symlink is followed, the path must lie inside one of the roots.
//! Relative paths join the main root; absolute paths must already point
//! into a root. Anything else is refused.
//!
//! The check reads the host filesystem, so it only applies to host
//! workspaces. A sandboxed workspace keeps the purely lexical rule, since
//! its paths name files in a sandbox this machine cannot inspect.

use std::path::{Component, Path, PathBuf};

use orca_harness_core::ToolError;

/// The main root (as spelled by the caller) and every root's canonical
/// form, the main root first.
#[derive(Debug)]
pub(super) struct Roots {
    main: PathBuf,
    canonical: Vec<PathBuf>,
}

impl Roots {
    pub(super) fn new(main: PathBuf) -> Self {
        let canonical = vec![canonical_or_lexical(&main)];
        Self { main, canonical }
    }

    /// Add host directories. Each must exist and be a directory; it is
    /// stored canonicalized, so a symlinked spelling cannot widen it later.
    pub(super) fn add(&mut self, dirs: impl IntoIterator<Item = PathBuf>) -> Result<(), ToolError> {
        for dir in dirs {
            let resolved = dir
                .canonicalize()
                .map_err(|e| ToolError::msg(format!("added directory {}: {e}", dir.display())))?;
            if !resolved.is_dir() {
                return Err(ToolError::msg(format!(
                    "added directory {} is not a directory",
                    dir.display()
                )));
            }
            if !self.canonical.contains(&resolved) {
                self.canonical.push(resolved);
            }
        }
        Ok(())
    }

    pub(super) fn main(&self) -> &Path {
        &self.main
    }

    /// The added directories, canonical, in the order given.
    pub(super) fn added(&self) -> &[PathBuf] {
        &self.canonical[1..]
    }

    /// Whether `dir` is one of the roots themselves, in either spelling.
    pub(super) fn is_root(&self, dir: &Path) -> bool {
        dir == self.main || self.canonical.iter().any(|root| root == dir)
    }

    /// Whether `path` lies inside a root once symlinks are followed.
    /// A dangling symlink anywhere on the path counts as outside: writing
    /// through it would create its target, wherever that is.
    pub(super) fn contains(&self, path: &Path) -> bool {
        let Ok(absolute) = std::path::absolute(path) else {
            return false;
        };
        let Some(normal) = normalize_absolute(&absolute) else {
            return false;
        };
        match canonical_prefix(&normal) {
            Some(real) => self.canonical.iter().any(|root| real.starts_with(root)),
            None => false,
        }
    }

    /// Resolve a caller-supplied path. `host` enables the absolute-path
    /// and symlink rules; without it this is the lexical check alone.
    pub(super) fn resolve(&self, raw: &str, host: bool) -> Result<PathBuf, ToolError> {
        let candidate = Path::new(raw);
        if candidate.is_absolute() {
            if !host {
                return Err(ToolError::msg(format!(
                    "path must be relative to the workspace root, got absolute: {raw}"
                )));
            }
            let normal = normalize_absolute(candidate)
                .ok_or_else(|| ToolError::msg(format!("unsupported path: {raw}")))?;
            return if self.contains(&normal) {
                Ok(normal)
            } else {
                Err(ToolError::msg(format!(
                    "path is outside the workspace and every added directory: {raw}"
                )))
            };
        }

        let joined = join_relative(&self.main, candidate, raw)?;
        // Checked against the canonical main root: the caller's spelling
        // may be relative (`.`) or run through a symlink (`/tmp`).
        if host && !self.contains(&join_relative(&self.canonical[0], candidate, raw)?) {
            return Err(ToolError::msg(format!(
                "path resolves outside the workspace through a symlink: {raw}"
            )));
        }
        Ok(joined)
    }
}

/// Join `rel` onto `root`, refusing any `..` that climbs above it. The
/// result is lexically normalized and touches no filesystem.
fn join_relative(root: &Path, rel: &Path, raw: &str) -> Result<PathBuf, ToolError> {
    let mut out = root.to_path_buf();
    let mut depth: isize = 0;
    for comp in rel.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(seg) => {
                out.push(seg);
                depth += 1;
            }
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(ToolError::msg(format!(
                        "path escapes the workspace root: {raw}"
                    )));
                }
                out.pop();
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ToolError::msg(format!(
                    "unsupported path component in {raw}"
                )));
            }
        }
    }
    Ok(out)
}

/// Fold `.` and `..` out of an absolute path without touching the
/// filesystem. `None` for a relative path.
fn normalize_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// The canonical form of `path`'s deepest existing ancestor with the
/// not-yet-existing tail re-appended, so files about to be created can
/// be checked too. `None` when an entry exists but cannot be resolved (a
/// dangling symlink or an unreadable link).
fn canonical_prefix(path: &Path) -> Option<PathBuf> {
    let mut tail = Vec::new();
    let mut current = path;
    loop {
        match current.canonicalize() {
            Ok(real) => {
                let mut out = real;
                out.extend(tail.iter().rev());
                return Some(out);
            }
            Err(_) if std::fs::symlink_metadata(current).is_ok() => return None,
            Err(_) => {
                tail.push(current.file_name()?.to_owned());
                current = current.parent()?;
            }
        }
    }
}

/// A root's canonical form, or the root itself when it does not exist
/// (tests root workspaces at made-up paths like `/work`).
fn canonical_or_lexical(root: &Path) -> PathBuf {
    let absolute = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    absolute
        .canonicalize()
        .ok()
        .or_else(|| canonical_prefix(&absolute))
        .unwrap_or(absolute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_dots_and_parents() {
        assert_eq!(
            normalize_absolute(Path::new("/a/./b/../c")),
            Some(PathBuf::from("/a/c"))
        );
        assert_eq!(normalize_absolute(Path::new("a/b")), None);
    }

    #[test]
    fn nonexistent_tail_is_reappended_to_the_real_ancestor() {
        let base = std::env::temp_dir().canonicalize().unwrap();
        let path = base.join("orca-roots-missing/x/y.txt");
        assert_eq!(canonical_prefix(&path), Some(path));
    }
}
