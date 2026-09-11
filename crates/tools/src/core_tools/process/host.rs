//! Typed host access to the `process` tool: the parameter and result
//! types its actions take, and [`ProcessController`], a handle that runs
//! the same implementation as the model-facing tool over the same
//! manager, so host- and model-started processes share ids, state,
//! output, and limits.

use std::sync::{Arc, Weak};
use std::time::Duration;

use orca_harness_core::{CancellationToken, ToolError};
use serde_json::{json, Value};

use super::{Manager, ProcessConfig, ProcessCore, ProcessTool};

/// Parameters of a `spawn`; defaults match the tool's JSON defaults
/// (`waitForExit` false, `notifyOnExit` true, no match pattern).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpawn {
    pub(super) command: String,
    pub(super) wait_for_exit: bool,
    pub(super) notify_on_exit: bool,
    pub(super) notify_on_match: Option<String>,
}

impl ProcessSpawn {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            wait_for_exit: false,
            notify_on_exit: true,
            notify_on_match: None,
        }
    }

    /// Return only once the process exits, with its final result; the
    /// call follows its cancellation token, the process does not.
    pub fn wait_for_exit(mut self, wait: bool) -> Self {
        self.wait_for_exit = wait;
        self
    }

    /// For a detached process, notify the host once when it exits.
    pub fn notify_on_exit(mut self, notify: bool) -> Self {
        self.notify_on_exit = notify;
        self
    }

    /// For a detached process, notify the host once when this literal
    /// first appears in output after the spawn result was taken.
    pub fn notify_on_match(mut self, pattern: impl Into<String>) -> Self {
        self.notify_on_match = Some(pattern.into());
        self
    }
}

/// Parameters of a `write` to a process's stdin; defaults match the
/// tool's JSON defaults (`newline` true, `eof` false).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessWrite {
    pub(super) input: String,
    pub(super) newline: bool,
    pub(super) eof: bool,
}

impl ProcessWrite {
    pub fn new(input: impl Into<String>) -> Self {
        Self {
            input: input.into(),
            newline: true,
            eof: false,
        }
    }

    /// Append a newline to `input`.
    pub fn newline(mut self, newline: bool) -> Self {
        self.newline = newline;
        self
    }

    /// Close stdin after writing, so the child sees EOF.
    pub fn eof(mut self, eof: bool) -> Self {
        self.eof = eof;
        self
    }
}

/// One process's state at an instant, with the output drained by the
/// call that produced it. The result of `spawn`, `poll`, `write`, and
/// `kill`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSnapshot {
    pub id: String,
    /// Up to `max_output_bytes` of unread output, now consumed.
    pub output: String,
    pub running: bool,
    /// The exit code once reaped; `None` while running or when the
    /// process died to a signal.
    pub exit_code: Option<i32>,
    /// Unread output remains after this drain.
    pub more_output: bool,
    /// Bytes discarded oldest-first since the previous drain because
    /// unread output exceeded the buffer cap; reported once.
    pub dropped_bytes: u64,
}

impl ProcessSnapshot {
    /// The JSON the `process` tool returns for the same state.
    pub fn into_value(self) -> Value {
        let mut out = json!({
            "id": self.id,
            "output": self.output,
            "running": self.running,
            "exitCode": self.exit_code,
            "moreOutput": self.more_output,
        });
        if self.dropped_bytes > 0 {
            out["droppedBytes"] = json!(self.dropped_bytes);
        }
        out
    }
}

/// One row of `list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEntry {
    pub id: String,
    /// The command line, truncated to 200 characters.
    pub command: String,
    pub running: bool,
    pub exit_code: Option<i32>,
}

impl ProcessEntry {
    /// The JSON the `process` tool returns for one row.
    pub fn into_value(self) -> Value {
        json!({
            "id": self.id,
            "command": self.command,
            "running": self.running,
            "exitCode": self.exit_code,
        })
    }

    /// The JSON the `process` tool returns for `list`.
    pub fn list_into_value(entries: Vec<ProcessEntry>) -> Value {
        let entries: Vec<Value> = entries.into_iter().map(Self::into_value).collect();
        json!({ "processes": entries })
    }
}

/// Typed host handle over a [`ProcessTool`]'s processes. Obtained from
/// [`ProcessTool::controller`]; every operation runs the code path the
/// model's `process` tool runs, against the same manager, so the two
/// observe the same ids and state.
///
/// The controller does not keep processes alive. It holds a weak
/// reference to the tool's manager: dropping the tool still kills its
/// children (and lets in-flight controller calls return), after which
/// every operation here fails with "process manager is closed" and
/// [`is_open`](Self::is_open) reports false.
#[derive(Clone)]
pub struct ProcessController {
    config: ProcessConfig,
    manager: Weak<Manager>,
}

impl ProcessController {
    pub(super) fn new(tool: &ProcessTool) -> Self {
        Self {
            config: tool.config.clone(),
            manager: Arc::downgrade(&tool.manager),
        }
    }

    /// False once the tool that owns the manager has been dropped.
    pub fn is_open(&self) -> bool {
        self.open().is_ok()
    }

    fn open(&self) -> Result<Arc<Manager>, ToolError> {
        self.manager
            .upgrade()
            .filter(|manager| !manager.shutdown.is_cancelled())
            .ok_or_else(|| ToolError::msg("process manager is closed"))
    }

    fn core<'a>(&'a self, manager: &'a Manager) -> ProcessCore<'a> {
        ProcessCore {
            config: &self.config,
            manager,
        }
    }

    /// Start a process; see [`ProcessSpawn`]. Cancelling `cancellation`
    /// abandons the call (a `wait_for_exit` wait in particular), never the
    /// process.
    pub async fn spawn(
        &self,
        spawn: ProcessSpawn,
        cancellation: CancellationToken,
    ) -> Result<ProcessSnapshot, ToolError> {
        let manager = self.open()?;
        self.core(&manager).spawn(spawn, &cancellation).await
    }

    /// Drain unread output, waiting up to `wait` for some to arrive when
    /// none is buffered (the tool's settle time by default, capped at
    /// its maximum wait).
    pub async fn poll(
        &self,
        id: &str,
        wait: Option<Duration>,
        cancellation: CancellationToken,
    ) -> Result<ProcessSnapshot, ToolError> {
        let manager = self.open()?;
        self.core(&manager).poll(id, wait, &cancellation).await
    }

    /// Write to a process's stdin; see [`ProcessWrite`].
    pub async fn write(
        &self,
        id: &str,
        write: ProcessWrite,
        cancellation: CancellationToken,
    ) -> Result<ProcessSnapshot, ToolError> {
        let manager = self.open()?;
        self.core(&manager).write(id, write, &cancellation).await
    }

    /// Terminate a process (its whole group) and forget it; the final
    /// snapshot carries its remaining output.
    pub async fn kill(&self, id: &str) -> Result<ProcessSnapshot, ToolError> {
        let manager = self.open()?;
        self.core(&manager).kill(id).await
    }

    /// Every known process, running or exited but not yet killed,
    /// sorted by id.
    pub fn list(&self) -> Result<Vec<ProcessEntry>, ToolError> {
        let manager = self.open()?;
        Ok(self.core(&manager).list())
    }
}
