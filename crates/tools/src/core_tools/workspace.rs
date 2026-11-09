//! Shared workspace root: every file tool resolves paths against it and
//! refuses to escape it, then performs its I/O through it.
//!
//! Added directories ([`Workspace::with_added_dirs`]) widen that boundary
//! to more host folders without moving the root; the rule for both lives
//! in [`roots`](super::roots).
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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use orca_harness_core::{Sandbox, Stat, ToolError};

use super::roots::Roots;

/// Where a workspace's file operations actually happen.
#[derive(Clone)]
enum Backend {
    Host,
    Sandbox(Arc<dyn Sandbox>),
}

/// A directory that file tools are rooted at. Cheap to clone (Arc'd).
#[derive(Clone)]
pub struct Workspace {
    roots: Arc<Roots>,
    backend: Backend,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("roots", &self.roots)
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
            roots: Arc::new(Roots::new(root.into())),
            backend: Backend::Host,
        }
    }

    /// Root the workspace at `root` *inside* `sandbox`. Every file
    /// operation goes to the provider; nothing touches this machine.
    pub fn sandboxed(root: impl Into<PathBuf>, sandbox: Arc<dyn Sandbox>) -> Self {
        Self {
            roots: Arc::new(Roots::new(root.into())),
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
        self.roots.main()
    }

    /// Resolve a caller-supplied path. Relative paths join the root and
    /// may not climb above it; absolute paths are accepted only when they
    /// point into the root or an added directory. On the host, existing
    /// symlinks are followed for the check, so a link cannot lead out of
    /// every root. The returned path is lexically normalized and may name
    /// a file that does not exist yet. See [`roots`](super::roots).
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, ToolError> {
        self.roots.resolve(rel, !self.is_sandboxed())
    }

    /// Whether an absolute host path lies inside the root or an added
    /// directory once symlinks are followed. Always `false` for a
    /// sandboxed workspace, whose paths this machine cannot inspect.
    pub fn contains(&self, path: &Path) -> bool {
        !self.is_sandboxed() && self.roots.contains(path)
    }

    /// Allow the file tools to reach these host directories too, by
    /// absolute path. Each must exist; it is stored canonicalized. The
    /// shell and every relative path still start at [`root`](Self::root).
    /// Refused for a sandboxed workspace.
    pub fn with_added_dirs<P: Into<PathBuf>>(
        self,
        dirs: impl IntoIterator<Item = P>,
    ) -> Result<Self, ToolError> {
        if self.is_sandboxed() {
            return Err(ToolError::msg(
                "added directories are host paths and cannot join a sandboxed workspace",
            ));
        }
        let mut roots = Roots::new(self.roots.main().to_path_buf());
        roots.add(self.roots.added().iter().cloned())?;
        roots.add(dirs.into_iter().map(Into::into))?;
        Ok(Self {
            roots: Arc::new(roots),
            backend: self.backend,
        })
    }

    /// The added directories, canonical, in the order given.
    pub fn added_dirs(&self) -> &[PathBuf] {
        self.roots.added()
    }

    /// Whether `dir` is the root or an added directory itself.
    pub(crate) fn is_root(&self, dir: &Path) -> bool {
        self.roots.is_root(dir)
    }

    /// The workspace-relative form of an absolute path, for display in
    /// tool output. Falls back to the input if it is not under the root,
    /// so files in added directories show their absolute path: the form
    /// the model must use to reach them again.
    pub fn display_rel<'a>(&self, path: &'a Path) -> &'a Path {
        path.strip_prefix(self.roots.main()).unwrap_or(path)
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

    /// [`stat`](Self::stat) for a check that must not pass on a failure:
    /// a sandbox that cannot say what is at `path` is an error here, not
    /// an absent file. The host keeps `stat`'s answer unchanged.
    pub(crate) async fn stat_checked(&self, path: &Path) -> Result<Option<Stat>, ToolError> {
        match &self.backend {
            Backend::Host => Ok(self.stat(path).await),
            Backend::Sandbox(sandbox) => sandbox
                .stat(&display(path))
                .await
                .map_err(|e| ToolError::msg(format!("stat failed: {e}"))),
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
