//! Shared workspace root: every file tool resolves paths against it and
//! refuses to escape it. This is defense-in-depth, not a sandbox — a
//! determined caller with a `shell` tool can still reach the whole host.
//! Real isolation is the platform's job (microVMs, the sandbox
//! extension); this just stops accidental `../../etc/passwd` reads.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use orca_harness_core::ToolError;

/// A directory that file tools are rooted at. Cheap to clone (Arc'd).
#[derive(Clone, Debug)]
pub struct Workspace {
    root: Arc<PathBuf>,
}

impl Workspace {
    /// Root the workspace at `root`. The path is used as-is; callers that
    /// want it canonicalized should canonicalize before constructing.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Arc::new(root.into()),
        }
    }

    /// Root at the current working directory.
    pub fn current_dir() -> Result<Self, ToolError> {
        std::env::current_dir()
            .map(Self::new)
            .map_err(|e| ToolError::msg(format!("cannot read current dir: {e}")))
    }

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// Resolve a caller-supplied relative path against the root, rejecting
    /// absolute paths and any `..` that would climb above the root. The
    /// returned path is lexically normalized (it does not touch the
    /// filesystem, so it is safe for not-yet-existing files).
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, ToolError> {
        let candidate = Path::new(rel);
        if candidate.is_absolute() {
            return Err(ToolError::msg(format!(
                "path must be relative to the workspace root, got absolute: {rel}"
            )));
        }

        let mut out = self.root.as_ref().clone();
        let mut depth: isize = 0;
        for comp in candidate.components() {
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
                            "path escapes the workspace root: {rel}"
                        )));
                    }
                    out.pop();
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(ToolError::msg(format!(
                        "unsupported path component in {rel}"
                    )));
                }
            }
        }
        Ok(out)
    }

    /// The workspace-relative form of an absolute path, for display in
    /// tool output. Falls back to the input if it is not under the root.
    pub fn display_rel<'a>(&self, path: &'a Path) -> &'a Path {
        path.strip_prefix(self.root.as_ref()).unwrap_or(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escape_and_absolute() {
        let ws = Workspace::new("/work");
        assert!(ws.resolve("../etc/passwd").is_err());
        assert!(ws.resolve("a/../../b").is_err());
        assert!(ws.resolve("/etc/passwd").is_err());
        assert_eq!(
            ws.resolve("a/b.txt").unwrap(),
            PathBuf::from("/work/a/b.txt")
        );
        assert_eq!(ws.resolve("./a/./b").unwrap(), PathBuf::from("/work/a/b"));
        // Descend then climb back to a legal sibling.
        assert_eq!(ws.resolve("a/../b").unwrap(), PathBuf::from("/work/b"));
    }
}
