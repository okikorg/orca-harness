//! `shell` — run a command and hand stdout/stderr/exit status to the model.
//!
//! Targeting: by default the command runs on the host via `sh -c "<cmd>"`.
//! Point it at another machine or container by setting an *executor*: a
//! program plus fixed leading args that receive the command string as the
//! final argument. `ssh user@host` runs on a remote box; `docker exec -i
//! <ctr> sh -c` runs inside a container. The tool's contract to the model
//! is identical either way — it asks for a command, it gets output.
//!
//! Cancellation: when the run's `CancellationToken` fires (e.g. the caller
//! disconnected), the child process is killed rather than left orphaned.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use orca_harness_core::{Tool, ToolContext, ToolError, ToolSchema};

use crate::pgroup;

/// How and where a shell command is executed.
#[derive(Clone, Debug)]
pub struct Executor {
    program: String,
    leading_args: Vec<String>,
}

impl Executor {
    /// The local host, via `sh -c "<cmd>"`. Deliberately not a login
    /// shell: profile scripts (`nvm`, banners) would pollute stdout that
    /// the model reads. Set a richer executor if you need profile PATH.
    pub fn local_sh() -> Self {
        Self {
            program: "sh".into(),
            leading_args: vec!["-c".into()],
        }
    }

    /// An arbitrary executor: `program` plus `leading_args`, with the
    /// command string appended as the final argument. Examples:
    /// `Executor::new("ssh", ["user@host"])`,
    /// `Executor::new("docker", ["exec","-i","ctr","sh","-lc"])`.
    pub fn new<I, S>(program: impl Into<String>, leading_args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            program: program.into(),
            leading_args: leading_args.into_iter().map(Into::into).collect(),
        }
    }

    /// Run `command` on a remote host over SSH.
    pub fn ssh(destination: impl Into<String>) -> Self {
        Self::new("ssh", [destination.into()])
    }

    /// Run `command` inside a running Docker container via `sh -c`.
    pub fn docker_exec(container: impl Into<String>) -> Self {
        Self::new(
            "docker",
            ["exec", "-i", &container.into(), "sh", "-c"].map(String::from),
        )
    }

    fn is_local_sh(&self) -> bool {
        self.program == "sh"
    }

    /// Build a [`Command`] that runs `command_str` through this executor.
    /// For the local `sh` executor `working_dir` becomes the child's real
    /// cwd; for remote executors there is no cwd to set, so it is folded
    /// in as `cd <dir> && …`. Stdio is left for the caller to configure.
    pub(crate) fn build(&self, command_str: &str, working_dir: Option<&str>) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.leading_args);
        let effective = match (working_dir, self.is_local_sh()) {
            (Some(_), true) | (None, _) => command_str.to_string(),
            (Some(dir), false) => format!("cd {dir} && {command_str}"),
        };
        cmd.arg(effective);
        if self.is_local_sh() {
            if let Some(dir) = working_dir {
                cmd.current_dir(dir);
            }
        }
        // Own process group: kills can reach grandchildren, and children
        // no longer die accidentally with the host's terminal — hosts
        // must kill them deliberately (see pgroup + CLI signal handling).
        #[cfg(unix)]
        cmd.process_group(0);
        cmd
    }
}

/// `shell` tool. Runs one command per call; concurrency-`Parallel` by
/// default (independent commands fan out), so serialize at the policy
/// layer if your target cannot handle concurrent sessions.
pub struct ShellTool {
    executor: Executor,
    /// Per-call wall-clock timeout. `None` relies on the run deadline.
    timeout: Option<Duration>,
    /// Cap on captured stdout/stderr bytes each, to protect the context
    /// window and the NDJSON line cap downstream.
    max_output_bytes: usize,
    working_dir: Option<String>,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new(Executor::local_sh())
    }
}

impl ShellTool {
    pub fn new(executor: Executor) -> Self {
        Self {
            executor,
            timeout: Some(Duration::from_secs(120)),
            max_output_bytes: 64 * 1024,
            working_dir: None,
        }
    }

    /// Convenience: a host shell.
    pub fn local() -> Self {
        Self::default()
    }

    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    /// Set a working directory. For the local executor this becomes the
    /// child's cwd; for remote executors it is prepended as `cd <dir> &&`.
    pub fn working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    fn build_command(&self, command_str: &str) -> Command {
        let mut cmd = self
            .executor
            .build(command_str, self.working_dir.as_deref());
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }
}

fn truncate(bytes: Vec<u8>, cap: usize) -> (String, bool) {
    let truncated = bytes.len() > cap;
    let slice = if truncated { &bytes[..cap] } else { &bytes[..] };
    (String::from_utf8_lossy(slice).into_owned(), truncated)
}

#[async_trait]
impl Tool for ShellTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "shell".into(),
            description: "Run a shell command and return its stdout, stderr, and exit code. \
                Use for building, testing, inspecting, and manipulating the target machine."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command line to execute."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let command_str = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`command` (string) is required"))?;
        if command_str.trim().is_empty() {
            // `sh -c ""` exits 0 doing nothing; succeeding silently would
            // tell the model its (missing) command worked.
            return Err(ToolError::msg("`command` must not be empty"));
        }

        let mut child = self
            .build_command(command_str)
            .spawn()
            .map_err(|e| ToolError::msg(format!("failed to spawn: {e}")))?;
        let pgid = child.id();

        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let read_streams = async {
            let mut out = Vec::new();
            let mut err = Vec::new();
            if let Some(pipe) = stdout_pipe.as_mut() {
                let _ = pipe.read_to_end(&mut out).await;
            }
            if let Some(pipe) = stderr_pipe.as_mut() {
                let _ = pipe.read_to_end(&mut err).await;
            }
            let status = child.wait().await;
            (out, err, status)
        };

        // Race execution against cancellation and the optional timeout.
        // On either, the whole process group is SIGKILLed so grandchildren
        // die too; `kill_on_drop` remains as the non-Unix fallback.
        let outcome = tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => {
                if let Some(pgid) = pgid {
                    pgroup::kill_group(pgid);
                }
                return Err(ToolError::msg("cancelled"));
            }
            result = async {
                match self.timeout {
                    Some(t) => tokio::time::timeout(t, read_streams).await.map_err(|_| ()),
                    None => Ok(read_streams.await),
                }
            } => result,
        };

        let (stdout, stderr, status) = match outcome {
            Ok(triple) => triple,
            Err(()) => {
                if let Some(pgid) = pgid {
                    pgroup::kill_group(pgid);
                }
                return Err(ToolError::msg("command timed out"));
            }
        };
        let status = status.map_err(|e| ToolError::msg(format!("wait failed: {e}")))?;

        let (stdout, stdout_truncated) = truncate(stdout, self.max_output_bytes);
        let (stderr, stderr_truncated) = truncate(stderr, self.max_output_bytes);

        Ok(json!({
            "stdout": stdout,
            "stderr": stderr,
            "exitCode": status.code(),
            "success": status.success(),
            "stdoutTruncated": stdout_truncated,
            "stderrTruncated": stderr_truncated,
        }))
    }
}
