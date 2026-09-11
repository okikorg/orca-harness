//! The agent-level process recipe and the session-level host handle over
//! background processes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::CancellationToken;
use orca_harness_tools::{
    Executor, ProcessController, ProcessEntry, ProcessSnapshot, ProcessSpawn, ProcessWrite,
};

use crate::SdkError;

/// How an agent's sessions run `shell` and `process` commands. Attach
/// with [`AgentBuilder::processes`](crate::AgentBuilder::processes);
/// only [`ToolPreset::Coding`](crate::ToolPreset::Coding) ships those
/// tools, so the builder rejects it for every other preset. Every
/// session (and every subagent child) builds its own tools from this
/// recipe, so nothing here is shared at run time.
#[derive(Clone, Debug, Default)]
pub struct ProcessConfig {
    pub(crate) executor: Option<Executor>,
    pub(crate) working_dir: Option<PathBuf>,
    pub(crate) max_output_bytes: Option<usize>,
    pub(crate) buffer_cap: Option<usize>,
    pub(crate) max_processes: Option<usize>,
}

impl ProcessConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `shell` and `process` commands through `executor` (an
    /// [`Executor::ssh`] or [`Executor::docker_exec`] target) instead of
    /// the local host shell. The file tools are not affected: they keep
    /// operating on the local workspace, so pair a remote executor with
    /// a synced or mounted workspace. Without an explicit
    /// [`working_dir`](Self::working_dir), commands on a remote executor
    /// run wherever the target starts them.
    pub fn executor(mut self, executor: Executor) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Where commands run. Defaults to the workspace root for the local
    /// executor and to nothing for a remote one, whose filesystem need
    /// not mirror the workspace.
    pub fn working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Cap on output bytes each `process` call returns (the tool's
    /// default when unset).
    pub fn max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = Some(bytes);
        self
    }

    /// Cap on unread output retained per process; older bytes beyond it
    /// are dropped and reported once (the tool's default when unset).
    pub fn buffer_cap(mut self, bytes: usize) -> Self {
        self.buffer_cap = Some(bytes);
        self
    }

    /// Cap on live processes per session, host- and model-started
    /// together (the tool's default when unset).
    pub fn max_processes(mut self, n: usize) -> Self {
        self.max_processes = Some(n);
        self
    }
}

/// Host handle over one session's background processes, when its tool
/// preset includes the `process` tool. Every operation runs the same
/// implementation as the session's `process` model tool over the same
/// session-owned manager, so host- and model-started processes share
/// ids, state, output limits, and the live process cap.
///
/// The handle keeps nothing alive: processes die with the session, and
/// [`Session::shutdown`] kills them and closes the manager, after which
/// every call here fails with [`SdkError::SessionClosed`] or
/// [`SdkError::Process`] and [`is_open`](Self::is_open) is false.
/// [`Session::clear`] kills them too but keeps the handle serving.
/// Process events (readiness matches, exits) reach the host through
/// [`Session::notifications`] as
/// [`BackgroundNotification::ProcessNotified`].
///
/// [`Session::shutdown`]: crate::Session::shutdown
/// [`Session::clear`]: crate::Session::clear
/// [`Session::notifications`]: crate::Session::notifications
/// [`BackgroundNotification::ProcessNotified`]: crate::BackgroundNotification::ProcessNotified
#[derive(Clone)]
pub struct Processes {
    controller: ProcessController,
    /// The session's shutdown flag; see [`Session::shutdown`].
    ///
    /// [`Session::shutdown`]: crate::Session::shutdown
    closed: Arc<AtomicBool>,
}

impl Processes {
    pub(crate) fn new(controller: ProcessController, closed: Arc<AtomicBool>) -> Self {
        Self { controller, closed }
    }

    fn ensure_open(&self) -> Result<(), SdkError> {
        if self.closed.load(Ordering::Acquire) {
            Err(SdkError::SessionClosed)
        } else {
            Ok(())
        }
    }

    /// False once the session was shut down or dropped.
    pub fn is_open(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && self.controller.is_open()
    }

    /// Start a process; see [`ProcessSpawn`]. The call's wait (a
    /// `wait_for_exit` spawn in particular) follows `cancellation` when
    /// one is given: cancelling abandons the call, never the process.
    /// Without a token the call can only end on its own.
    pub async fn spawn(
        &self,
        spawn: ProcessSpawn,
        cancellation: Option<CancellationToken>,
    ) -> Result<ProcessSnapshot, SdkError> {
        self.ensure_open()?;
        self.controller
            .spawn(spawn, cancellation.unwrap_or_default())
            .await
            .map_err(process_error)
    }

    /// Drain unread output, waiting up to `wait` for some to arrive when
    /// none is buffered (the tool's default settle time when `None`).
    pub async fn poll(
        &self,
        id: &str,
        wait: Option<Duration>,
    ) -> Result<ProcessSnapshot, SdkError> {
        self.ensure_open()?;
        self.controller
            .poll(id, wait, CancellationToken::new())
            .await
            .map_err(process_error)
    }

    /// Write to a process's stdin; see [`ProcessWrite`].
    pub async fn write(&self, id: &str, write: ProcessWrite) -> Result<ProcessSnapshot, SdkError> {
        self.ensure_open()?;
        self.controller
            .write(id, write, CancellationToken::new())
            .await
            .map_err(process_error)
    }

    /// Terminate a process and forget it; the snapshot carries its
    /// remaining output.
    pub async fn kill(&self, id: &str) -> Result<ProcessSnapshot, SdkError> {
        self.ensure_open()?;
        self.controller.kill(id).await.map_err(process_error)
    }

    /// Every known process, running or exited but not yet killed,
    /// sorted by id.
    pub fn list(&self) -> Result<Vec<ProcessEntry>, SdkError> {
        self.ensure_open()?;
        self.controller.list().map_err(process_error)
    }
}

fn process_error(error: orca_harness_core::ToolError) -> SdkError {
    SdkError::Process(error.to_string())
}
