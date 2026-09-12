//! Where a long-lived process runs.
//!
//! `py_kernel` and `bun_repl` keep an interpreter alive across calls and
//! drive it with framed writes to stdin, reading stdout until a sentinel.
//! That shape is identical whether the interpreter is a child process on
//! this machine or a process inside a sandbox — only the plumbing differs,
//! and this is the plumbing.
//!
//! Output is a channel rather than an `AsyncRead` because that is the one
//! shape both sides can offer: a sandbox session has no file descriptor to
//! hand back. The local side pumps its pipes into the same channel, which
//! is what `bun_repl` already did with its own pipes.

use std::process::Stdio;
use std::sync::Arc;

use orca_harness_core::{Chunk, Sandbox, SandboxError, SpawnRequest, ToolError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

/// Backpressure the readers instead of letting a runaway program queue
/// output without limit before the caller can truncate it.
const OUTPUT_CHANNEL_CHUNKS: usize = 32;

/// The output stream of a [`Spawned`] process, stdout and stderr
/// interleaved in arrival order.
pub type Output = mpsc::Receiver<Chunk>;

/// Where [`Spawned`] processes are started.
#[derive(Clone)]
pub enum Spawner {
    /// A child process on this machine, in its own process group.
    Local,
    /// A process inside a sandbox, reached through the provider.
    Sandbox(Arc<dyn Sandbox>),
}

impl std::fmt::Debug for Spawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Spawner::Local => "Spawner::Local",
            Spawner::Sandbox(_) => "Spawner::Sandbox",
        })
    }
}

/// What to start.
pub struct Spawn {
    pub program: String,
    pub args: Vec<String>,
    pub working_dir: Option<String>,
    /// Merge stderr into the output stream. When false, stderr is
    /// discarded — `py_kernel`'s driver already folds stderr into stdout
    /// at the fd level to preserve ordering.
    pub capture_stderr: bool,
}

impl Spawner {
    pub fn is_sandboxed(&self) -> bool {
        matches!(self, Spawner::Sandbox(_))
    }

    pub async fn spawn(&self, spawn: Spawn) -> Result<(Spawned, Output), ToolError> {
        match self {
            Spawner::Local => Self::spawn_local(spawn),
            Spawner::Sandbox(sandbox) => Self::spawn_sandboxed(sandbox, spawn).await,
        }
    }

    fn spawn_local(spawn: Spawn) -> Result<(Spawned, Output), ToolError> {
        let mut cmd = tokio::process::Command::new(&spawn.program);
        cmd.args(&spawn.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if spawn.capture_stderr {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .kill_on_drop(true);
        if let Some(dir) = &spawn.working_dir {
            cmd.current_dir(dir);
        }
        // Own process group: kills reach grandchildren, and children no
        // longer die with the host's terminal.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::msg(format!("failed to spawn {}: {e}", spawn.program)))?;
        let pgid = child.id();
        let stdin = child.stdin.take().expect("stdin piped");

        let (tx, rx) = mpsc::channel(OUTPUT_CHANNEL_CHUNKS);
        if let Some(pipe) = child.stdout.take() {
            pump(pipe, tx.clone(), false);
        }
        if let Some(pipe) = child.stderr.take() {
            // Tagged, not merged: callers that frame the two streams
            // separately (bun_repl waits on a sentinel in each) need to
            // tell them apart.
            pump(pipe, tx, true);
        }

        Ok((
            Spawned {
                inner: Inner::Local { child, stdin },
                pgid,
            },
            rx,
        ))
    }

    async fn spawn_sandboxed(
        sandbox: &Arc<dyn Sandbox>,
        spawn: Spawn,
    ) -> Result<(Spawned, Output), ToolError> {
        if !sandbox.capabilities().sessions {
            return Err(ToolError::msg(format!(
                "this sandbox cannot hold a long-lived process, which {} requires",
                spawn.program
            )));
        }
        let request = SpawnRequest {
            program: spawn.program,
            args: spawn.args,
            cwd: spawn.working_dir,
            env: Vec::new(),
        };
        let (session, output) = sandbox
            .spawn(request)
            .await
            .map_err(|e: SandboxError| ToolError::msg(e.to_string()))?;
        Ok((
            Spawned {
                inner: Inner::Sandbox(session),
                pgid: None,
            },
            output,
        ))
    }
}

enum Inner {
    Local { child: Child, stdin: ChildStdin },
    Sandbox(Box<dyn orca_harness_core::Session>),
}

/// A live process, wherever it runs.
pub struct Spawned {
    inner: Inner,
    /// The local process group, for the synchronous kill a `Drop` impl
    /// can perform. `None` for a sandboxed process, whose lifetime the
    /// provider owns.
    pgid: Option<u32>,
}

impl Spawned {
    pub fn pgid(&self) -> Option<u32> {
        self.pgid
    }

    /// Write one framed message. An error means the process is gone and
    /// the caller should restart it.
    pub async fn write_stdin(&mut self, bytes: &[u8]) -> Result<(), ToolError> {
        match &mut self.inner {
            Inner::Local { stdin, .. } => stdin
                .write_all(bytes)
                .await
                .and(stdin.flush().await)
                .map_err(|e| ToolError::msg(format!("stdin write failed: {e}"))),
            Inner::Sandbox(session) => session
                .write_stdin(bytes)
                .await
                .map_err(|e| ToolError::msg(e.to_string())),
        }
    }

    /// Whether the process has already exited on its own. A local child
    /// is reaped here; a sandboxed one reports `false` because the
    /// provider does not surface exit without a round trip, and the
    /// callers detect death through a failed write or a closed stream.
    pub fn has_exited(&mut self) -> bool {
        match &mut self.inner {
            Inner::Local { child, .. } => child.try_wait().ok().flatten().is_some(),
            Inner::Sandbox(_) => false,
        }
    }

    /// Terminate and reap. Kills the whole process group locally so
    /// grandchildren die too.
    pub async fn kill(&mut self) {
        match &mut self.inner {
            Inner::Local { child, .. } => {
                if let Some(pgid) = self.pgid {
                    crate::pgroup::kill_group(pgid);
                }
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            Inner::Sandbox(session) => {
                let _ = session.kill().await;
            }
        }
    }
}

fn pump<R>(mut pipe: R, tx: mpsc::Sender<Chunk>, stderr: bool)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx
                        .send(Chunk {
                            stderr,
                            bytes: buf[..n].to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
}
