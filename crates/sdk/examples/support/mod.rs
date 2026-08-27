//! Shared test and example support helpers.
//!
//! Provides an RAII [`TempWorkspace`] helper to create an isolated, unique
//! temporary directory for Harness instances and automatically clean it up
//! on drop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// RAII wrapper around a unique temporary directory for hermetic harness runs.
pub struct TempWorkspace {
    path: PathBuf,
}

impl TempWorkspace {
    /// Creates a new unique temporary directory prefixed with `label`.
    pub fn new(label: &str) -> Self {
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let path =
            std::env::temp_dir().join(format!("orca-example-{label}-{pid}-{timestamp}-{count}"));
        std::fs::create_dir_all(&path).expect("failed to create temp workspace directory");
        Self { path }
    }

    /// Returns the path to the temporary workspace directory.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempWorkspace {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
