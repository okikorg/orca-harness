//! The session-level host handle over background processes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::CancellationToken;
use orca_harness_tools::{
    ProcessController, ProcessEntry, ProcessSnapshot, ProcessSpawn, ProcessWrite,
};

use crate::SdkError;

/// Host handle over one session's background processes, when its tool
/// preset includes the `process` tool. Every operation runs the same
/// implementation as the session's `process` model tool over the same
/// session-owned manager, so host- and model-started processes share
/// ids, state, output limits, and the live process cap.
///
/// The handle keeps nothing alive: processes die with the session (or
/// its explicit shutdown refuses further operations), after which every
/// call here fails with [`SdkError::SessionClosed`] or
/// [`SdkError::Process`] and [`is_open`](Self::is_open) is false.
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

    /// Start a process; see [`ProcessSpawn`]. A `wait_for_exit` spawn
    /// returns when the process exits and cannot be interrupted; use
    /// [`spawn_with`](Self::spawn_with) to bound the wait.
    pub async fn spawn(&self, spawn: ProcessSpawn) -> Result<ProcessSnapshot, SdkError> {
        self.spawn_with(spawn, CancellationToken::new()).await
    }

    /// [`spawn`](Self::spawn) whose wait follows `cancellation`:
    /// cancelling abandons the call, never the process.
    pub async fn spawn_with(
        &self,
        spawn: ProcessSpawn,
        cancellation: CancellationToken,
    ) -> Result<ProcessSnapshot, SdkError> {
        self.ensure_open()?;
        self.controller
            .spawn(spawn, cancellation)
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
