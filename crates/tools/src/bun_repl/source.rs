//! The per-call source file `.load` reads, written next to the interpreter
//! and removed on drop.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use orca_harness_core::ToolError;

use super::TEMP_SEQ;

/// Removes the per-call source even when execution is cancelled or times out.
///
/// The file has to live wherever `bun` does: `.load` is executed *by the
/// REPL*, so a host temp file is invisible to a sandboxed interpreter.
/// Under a sandbox the source is written through the provider and removed
/// the same way.
pub(super) struct TempSource {
    pub(super) path: PathBuf,
    /// Set when the file lives in a sandbox, so `Drop` knows where to
    /// delete it from.
    pub(super) sandbox: Option<Arc<dyn orca_harness_core::Sandbox>>,
}

impl TempSource {
    /// The per-call file name. Process id plus sequence, so two tools in
    /// one process never collide and neither do two processes.
    fn file_name(seq: u64) -> String {
        format!("orca-bun-repl-{}-{seq}.ts", std::process::id())
    }

    fn body(code: &str, marker: &str) -> Vec<u8> {
        let mut out = code.as_bytes().to_vec();
        out.extend_from_slice(b"\n;console.log(");
        out.extend_from_slice(marker.as_bytes());
        out.extend_from_slice(b")\n");
        out
    }

    /// Write the source inside `sandbox`, in the directory `bun` runs in.
    pub(super) async fn write_sandboxed(
        sandbox: &Arc<dyn orca_harness_core::Sandbox>,
        dir: &str,
        code: &str,
        marker: &str,
    ) -> Result<Self, ToolError> {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let path = PathBuf::from(dir).join(Self::file_name(seq));
        sandbox
            .write_file(
                &path.to_string_lossy(),
                &Self::body(code, marker),
                orca_harness_core::FileMode::Regular,
            )
            .await
            .map_err(|error: orca_harness_core::SandboxError| {
                ToolError::msg(format!("failed to write Bun REPL input: {error}"))
            })?;
        Ok(Self {
            path,
            sandbox: Some(sandbox.clone()),
        })
    }

    pub(super) fn write(code: &str, marker: &str) -> Result<Self, ToolError> {
        let dir = std::env::temp_dir();
        if dir.to_string_lossy().chars().any(char::is_whitespace) {
            return Err(ToolError::msg(
                "bun_repl needs a temporary directory whose path has no whitespace",
            ));
        }
        for _ in 0..16 {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::SeqCst);
            let path = dir.join(Self::file_name(seq));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(&path);
            match file {
                Ok(mut file) => {
                    let source = Self {
                        path,
                        sandbox: None,
                    };
                    file.write_all(code.as_bytes())
                        .and_then(|_| file.write_all(b"\n;console.log("))
                        .and_then(|_| file.write_all(marker.as_bytes()))
                        .and_then(|_| file.write_all(b")\n"))
                        .map_err(|error| {
                            ToolError::msg(format!("failed to write Bun REPL input: {error}"))
                        })?;
                    return Ok(source);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(ToolError::msg(format!(
                        "failed to create Bun REPL input: {error}"
                    )))
                }
            }
        }
        Err(ToolError::msg(
            "failed to allocate a unique Bun REPL input file",
        ))
    }
}

impl Drop for TempSource {
    fn drop(&mut self) {
        let Some(sandbox) = self.sandbox.take() else {
            let _ = std::fs::remove_file(&self.path);
            return;
        };
        // A provider delete is async and `Drop` is not, so it is handed
        // to the runtime. The file is one small per-call source; losing
        // the race at shutdown leaks it inside a sandbox that is about to
        // be destroyed anyway.
        let path = std::mem::take(&mut self.path);
        tokio::spawn(async move {
            let command = format!("rm -f '{}'", path.to_string_lossy().replace('\'', r"'\''"));
            let _ = sandbox
                .exec(orca_harness_core::ExecRequest::new(command))
                .await;
        });
    }
}
