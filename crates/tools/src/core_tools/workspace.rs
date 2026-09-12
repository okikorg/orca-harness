//! Shared workspace root: every file tool resolves paths against it and
//! refuses to escape it, then performs its I/O through it.
//!
//! Path resolution alone is defense-in-depth, not a sandbox — a caller
//! with a `shell` tool can still reach the whole host, and this just
//! stops accidental `../../etc/passwd` reads. Real isolation comes from
//! the *backend*: point a workspace at a
//! [`Sandbox`](orca_harness_core::Sandbox) with [`Workspace::sandboxed`]
//! and every file tool rooted at it reads and writes inside that sandbox
//! instead of on this machine.
//!
//! The backend lives here rather than in each tool for one reason: eight
//! tools share this type, and a boundary that eight places have to
//! remember to honor is a boundary with seven chances to be forgotten.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use orca_harness_core::{Sandbox, Stat, ToolError};

/// Where a workspace's file operations actually happen.
#[derive(Clone)]
enum Backend {
    Host,
    Sandbox(Arc<dyn Sandbox>),
}

/// A directory that file tools are rooted at. Cheap to clone (Arc'd).
#[derive(Clone)]
pub struct Workspace {
    root: Arc<PathBuf>,
    backend: Backend,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("root", &self.root)
            .field(
                "backend",
                match self.backend {
                    Backend::Host => &"host",
                    Backend::Sandbox(_) => &"sandbox",
                },
            )
            .finish()
    }
}

impl Workspace {
    /// Root the workspace at `root` on this machine. The path is used
    /// as-is; callers that want it canonicalized should canonicalize
    /// before constructing.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Arc::new(root.into()),
            backend: Backend::Host,
        }
    }

    /// Root the workspace at `root` *inside* `sandbox`. Every file
    /// operation goes to the provider; nothing touches this machine.
    pub fn sandboxed(root: impl Into<PathBuf>, sandbox: Arc<dyn Sandbox>) -> Self {
        Self {
            root: Arc::new(root.into()),
            backend: Backend::Sandbox(sandbox),
        }
    }

    /// Whether this workspace's files live in a sandbox rather than on
    /// the host. Tool assembly reads this to refuse combinations that
    /// would straddle the boundary.
    pub fn is_sandboxed(&self) -> bool {
        matches!(self.backend, Backend::Sandbox(_))
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

/// The I/O facade. Every file tool goes through these instead of
/// `tokio::fs`, so the host and sandbox backends cannot drift apart and
/// no tool can reach the host by accident.
///
/// Paths are the already-resolved absolute form from [`Workspace::resolve`];
/// under a sandbox they are absolute inside that sandbox.
impl Workspace {
    pub(crate) async fn read(&self, path: &Path) -> Result<Vec<u8>, ToolError> {
        match &self.backend {
            Backend::Host => tokio::fs::read(path)
                .await
                .map_err(|e| ToolError::msg(format!("read failed: {e}"))),
            Backend::Sandbox(sandbox) => sandbox
                .read_file(&display(path))
                .await
                .map_err(|e| ToolError::msg(format!("read failed: {e}"))),
        }
    }

    pub(crate) async fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
        match &self.backend {
            Backend::Host => {
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| ToolError::msg(format!("mkdir failed: {e}")))?;
                }
                tokio::fs::write(path, bytes)
                    .await
                    .map_err(|e| ToolError::msg(format!("write failed: {e}")))
            }
            // The provider creates parent directories itself; asking it
            // to do so separately would be a second round trip.
            Backend::Sandbox(sandbox) => sandbox
                .write_file(&display(path), bytes, orca_harness_core::FileMode::Regular)
                .await
                .map_err(|e| ToolError::msg(format!("write failed: {e}"))),
        }
    }

    /// What the file looks like now, or `None` if it is absent or
    /// unreadable. Absence is not an error: callers ask precisely because
    /// they do not know, and creating a new file is always allowed.
    pub(crate) async fn stat(&self, path: &Path) -> Option<Stat> {
        match &self.backend {
            Backend::Host => tokio::fs::metadata(path).await.ok().map(|meta| Stat {
                modified: meta.modified().ok(),
                len: meta.len(),
                is_dir: meta.is_dir(),
            }),
            Backend::Sandbox(sandbox) => sandbox.stat(&display(path)).await.ok().flatten(),
        }
    }

    /// Delete a file. A path that is already absent is success, matching
    /// `rm -f` — callers use this to roll back a change that may never
    /// have been written.
    pub(crate) async fn remove_file(&self, path: &Path) -> Result<(), ToolError> {
        match &self.backend {
            Backend::Host => match tokio::fs::remove_file(path).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(ToolError::msg(format!("remove failed: {e}"))),
            },
            Backend::Sandbox(sandbox) => {
                let request = orca_harness_core::ExecRequest::new(format!(
                    "rm -f {}",
                    crate::core_tools::shell_quote(&display(path))
                ));
                let output = sandbox
                    .exec(request)
                    .await
                    .map_err(|e| ToolError::msg(format!("remove failed: {e}")))?;
                match output.exit_code {
                    0 => Ok(()),
                    _ => Err(ToolError::msg(format!(
                        "remove failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ))),
                }
            }
        }
    }

    /// Read a file that may not exist: `None` means absent, which is a
    /// normal answer here rather than a failure.
    pub(crate) async fn read_opt(&self, path: &Path) -> Result<Option<Vec<u8>>, ToolError> {
        match &self.backend {
            Backend::Host => match tokio::fs::read(path).await {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(ToolError::msg(format!("read failed: {e}"))),
            },
            // No error kinds to match on across providers, so absence is
            // established before reading rather than inferred afterwards.
            Backend::Sandbox(_) => match self.stat(path).await {
                None => Ok(None),
                Some(_) => self.read(path).await.map(Some),
            },
        }
    }

    /// One directory's entries as `(name, is_dir)`, unsorted.
    pub(crate) async fn list(&self, path: &Path) -> Result<Vec<(String, bool)>, ToolError> {
        match &self.backend {
            Backend::Host => {
                let mut reader = tokio::fs::read_dir(path)
                    .await
                    .map_err(|e| ToolError::msg(format!("read_dir failed: {e}")))?;
                let mut out = Vec::new();
                while let Some(entry) = reader
                    .next_entry()
                    .await
                    .map_err(|e| ToolError::msg(format!("read_dir failed: {e}")))?
                {
                    let is_dir = entry
                        .file_type()
                        .await
                        .map(|kind| kind.is_dir())
                        .unwrap_or(false);
                    out.push((entry.file_name().to_string_lossy().into_owned(), is_dir));
                }
                Ok(out)
            }
            Backend::Sandbox(sandbox) => sandbox
                .list_dir(&display(path))
                .await
                .map(|entries| {
                    entries
                        .into_iter()
                        .map(|entry| (entry.name, entry.is_dir))
                        .collect()
                })
                .map_err(|e| ToolError::msg(format!("read_dir failed: {e}"))),
        }
    }
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
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
