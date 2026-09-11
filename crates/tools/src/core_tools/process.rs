//! `process` — persistent sessions and background processes.
//!
//! Where `shell` runs one command to completion per call, `process` keeps
//! children alive across calls: start a dev server or build watcher and
//! poll its output, or drive an interactive REPL (`python3 -i`, `psql`)
//! by writing to its stdin. One tool, five actions: `spawn`, `poll`,
//! `write`, `kill`, `list`.
//!
//! Output handling: stdout and stderr are merged into a bounded unread
//! buffer per process (terminal semantics). Each response drains up to
//! `max_output_bytes` of it; anything beyond `buffer_cap` waiting unread
//! is dropped oldest-first and reported via `droppedBytes`.
//!
//! Lifetime: children are killed when the tool is dropped (its shutdown
//! token closes the manager), and a run's cancellation only interrupts
//! the current call —
//! processes deliberately survive between calls. There is no PTY here;
//! programs that refuse to run without one need the host to provide a
//! richer executor.
//!
//! Hosts get the same five actions, typed, through
//! [`ProcessController`] (see [`ProcessTool::controller`]): one
//! implementation ([`ProcessCore`]) serves both the JSON tool and the
//! controller, so neither path can drift from the other.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::Notify;

use orca_harness_core::{CancellationToken, Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use crate::pgroup;
use crate::shell::Executor;
use crate::BackgroundStats;

mod host;
mod output;
mod spawn;

pub use host::{ProcessController, ProcessEntry, ProcessSnapshot, ProcessSpawn, ProcessWrite};
use output::OutBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessNotificationKind {
    OutputMatch { pattern: String },
    Exit { exit_code: Option<i32> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessNotification {
    pub id: String,
    pub command: String,
    pub kind: ProcessNotificationKind,
    pub output: String,
    pub dropped_bytes: u64,
    pub more_output: bool,
}

type ProcessNotifier = Arc<dyn Fn(ProcessNotification) + Send + Sync>;

struct Proc {
    id: String,
    command: String,
    /// Process-group id (== child pid, since each child leads its group).
    pgid: Option<u32>,
    /// True while this child is counted in the shared stats; whoever
    /// swaps it to false performs the single decrement (waiter on reap,
    /// or `Manager::drop` for children the waiter never reaps).
    counted: AtomicBool,
    buf: Mutex<OutBuf>,
    /// Wakes pollers when the readers append output.
    output_ready: Notify,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    /// `Some(exit_code)` once the child has been reaped.
    exit: Mutex<Option<Option<i32>>>,
    /// Cancel to kill the child. A child token of the manager's shutdown.
    kill: CancellationToken,
    /// Fires after exit, once the readers have (briefly) drained.
    done: CancellationToken,
    notify_on_exit: AtomicBool,
    notifier: Option<ProcessNotifier>,
}

impl Proc {
    fn running(&self) -> bool {
        self.exit.lock().unwrap().is_none()
    }
    fn exit_code(&self) -> Option<i32> {
        self.exit.lock().unwrap().flatten()
    }

    fn notification(&self, kind: ProcessNotificationKind) -> ProcessNotification {
        let (output, dropped_bytes) = self.buf.lock().unwrap().drain_notification();
        ProcessNotification {
            id: self.id.clone(),
            command: self.command.clone(),
            kind,
            output,
            dropped_bytes,
            more_output: false,
        }
    }

    fn emit(&self, kind: ProcessNotificationKind) {
        if let Some(notifier) = &self.notifier {
            notifier(self.notification(kind));
        }
    }

    fn append_output(&self, chunk: &[u8]) {
        let matched = self.buf.lock().unwrap().push(chunk);
        if let Some(pattern) = matched {
            self.emit(ProcessNotificationKind::OutputMatch { pattern });
        }
        self.output_ready.notify_waiters();
    }
}

struct Manager {
    seq: AtomicU64,
    procs: Mutex<HashMap<String, Arc<Proc>>>,
    shutdown: CancellationToken,
    stats: BackgroundStats,
}

impl Manager {
    fn new(stats: BackgroundStats) -> Arc<Self> {
        Arc::new(Self {
            seq: AtomicU64::new(0),
            procs: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
            stats,
        })
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        // Cancel wakes the waiters if the runtime still lives; the direct
        // group kills guarantee cleanup even when it does not (runtime
        // shutdown aborts waiter tasks before they can act).
        self.shutdown.cancel();
        for proc in self.procs.lock().unwrap().values() {
            if proc.running() {
                if let Some(pgid) = proc.pgid {
                    pgroup::kill_group(pgid);
                }
            }
            if proc.counted.swap(false, Ordering::Relaxed) {
                self.stats.remove_process(&proc.id);
                self.stats.dec_processes();
            }
        }
    }
}

/// The tool's settings, shared by value with its controllers.
#[derive(Clone)]
struct ProcessConfig {
    executor: Executor,
    working_dir: Option<String>,
    /// Cap on output bytes returned per call.
    max_output_bytes: usize,
    /// Cap on unread output retained per process.
    buffer_cap: usize,
    /// How long spawn/write linger for output before responding.
    settle: Duration,
    /// Upper bound on a poll's `waitMs`.
    max_wait: Duration,
    max_processes: usize,
    notifier: Option<ProcessNotifier>,
}

/// The single implementation of the five actions, borrowed from either
/// the tool or a controller that upgraded its manager reference.
struct ProcessCore<'a> {
    config: &'a ProcessConfig,
    manager: &'a Manager,
}

impl ProcessCore<'_> {
    fn get(&self, id: &str) -> Result<Arc<Proc>, ToolError> {
        self.manager
            .procs
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| ToolError::msg(format!("unknown process id: {id}")))
    }

    fn snapshot(&self, id: &str, proc: &Proc, arm_exit: bool, arm_match: bool) -> ProcessSnapshot {
        // Read the exit slot once, and before draining: `running` and
        // `exitCode` must describe the same instant, and reading liveness
        // first means an exit racing this snapshot shows up as "still
        // running, output partial" (the next poll completes the story)
        // rather than "exited" with output still in flight.
        let exit_guard = proc.exit.lock().unwrap();
        let exit = *exit_guard;
        let mut buf = proc.buf.lock().unwrap();
        let (output, dropped, more) = buf.drain(self.config.max_output_bytes);
        if arm_match {
            // The spawn result already carries everything drained above.
            // Start autonomous delivery after that exact boundary so output
            // cannot both complete spawn and wake the model again.
            buf.arm_notifications();
        }
        if arm_exit && exit.is_none() {
            proc.notify_on_exit.store(true, Ordering::Release);
        }
        drop(exit_guard);
        ProcessSnapshot {
            id: id.to_string(),
            output,
            running: exit.is_none(),
            exit_code: exit.flatten(),
            more_output: more,
            dropped_bytes: dropped,
        }
    }

    async fn poll(
        &self,
        id: &str,
        wait: Option<Duration>,
        cancellation: &CancellationToken,
    ) -> Result<ProcessSnapshot, ToolError> {
        let proc = self.get(id)?;
        let wait = wait.unwrap_or(self.config.settle).min(self.config.max_wait);

        let notified = proc.output_ready.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let idle = proc.buf.lock().unwrap().is_empty();
        if idle && proc.running() {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ToolError::msg("cancelled")),
                _ = proc.done.cancelled() => {}
                _ = &mut notified => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
        settle_exit(&proc).await;
        Ok(self.snapshot(id, &proc, false, false))
    }

    async fn write(
        &self,
        id: &str,
        write: ProcessWrite,
        cancellation: &CancellationToken,
    ) -> Result<ProcessSnapshot, ToolError> {
        let proc = self.get(id)?;
        if !proc.running() {
            return Err(ToolError::msg(format!("process {id} has exited")));
        }
        {
            let mut guard = proc.stdin.lock().await;
            let stdin = guard
                .as_mut()
                .ok_or_else(|| ToolError::msg(format!("stdin of {id} is closed")))?;
            let data = if write.newline {
                format!("{}\n", write.input)
            } else {
                write.input
            };
            stdin
                .write_all(data.as_bytes())
                .await
                .map_err(|e| ToolError::msg(format!("write to stdin failed: {e}")))?;
            stdin
                .flush()
                .await
                .map_err(|e| ToolError::msg(format!("flush stdin failed: {e}")))?;
            if write.eof {
                *guard = None; // drop the handle → child sees EOF
            }
        }

        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ToolError::msg("cancelled")),
            _ = proc.done.cancelled() => {}
            _ = tokio::time::sleep(self.config.settle) => {}
        }
        settle_exit(&proc).await;
        Ok(self.snapshot(id, &proc, false, false))
    }

    async fn kill(&self, id: &str) -> Result<ProcessSnapshot, ToolError> {
        let proc = self
            .manager
            .procs
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(|| ToolError::msg(format!("unknown process id: {id}")))?;
        terminate(&proc).await;
        Ok(self.snapshot(id, &proc, false, false))
    }

    fn list(&self) -> Vec<ProcessEntry> {
        let procs = self.manager.procs.lock().unwrap();
        let mut entries: Vec<ProcessEntry> = procs
            .iter()
            .map(|(id, p)| ProcessEntry {
                id: id.clone(),
                command: p.command.chars().take(200).collect(),
                running: p.running(),
                exit_code: p.exit_code(),
            })
            .collect();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
    }
}

pub struct ProcessTool {
    config: ProcessConfig,
    manager: Arc<Manager>,
}

impl Default for ProcessTool {
    fn default() -> Self {
        Self::new(Executor::local_sh())
    }
}

impl ProcessTool {
    pub fn new(executor: Executor) -> Self {
        Self {
            config: ProcessConfig {
                executor,
                working_dir: None,
                max_output_bytes: 64 * 1024,
                buffer_cap: 512 * 1024,
                settle: Duration::from_millis(500),
                max_wait: Duration::from_secs(30),
                max_processes: 32,
                notifier: None,
            },
            manager: Manager::new(BackgroundStats::default()),
        }
    }

    /// Adopt shared live counters. This replaces the manager, so it
    /// must come first among the builders that touch it.
    ///
    /// # Panics
    ///
    /// Must be called before [`controller`](Self::controller) and before
    /// any spawn: a controller taken earlier would watch the discarded
    /// manager and report closed, and a process started earlier would be
    /// orphaned by the swap.
    pub fn stats(mut self, stats: BackgroundStats) -> Self {
        assert!(
            Arc::weak_count(&self.manager) == 0 && self.manager.procs.lock().unwrap().is_empty(),
            "ProcessTool::stats must be called before controller() and before any spawn"
        );
        self.manager = Manager::new(stats);
        self
    }

    /// Convenience: host-local processes.
    pub fn local() -> Self {
        Self::default()
    }

    pub fn working_dir(mut self, dir: impl Into<String>) -> Self {
        self.config.working_dir = Some(dir.into());
        self
    }

    pub fn max_output_bytes(mut self, bytes: usize) -> Self {
        self.config.max_output_bytes = bytes;
        self
    }

    /// Cap on unread output retained per process; older bytes beyond it
    /// are dropped and reported once as `droppedBytes`.
    pub fn buffer_cap(mut self, bytes: usize) -> Self {
        self.config.buffer_cap = bytes;
        self
    }

    pub fn max_processes(mut self, n: usize) -> Self {
        self.config.max_processes = n;
        self
    }

    pub fn on_notification(
        mut self,
        notify: impl Fn(ProcessNotification) + Send + Sync + 'static,
    ) -> Self {
        self.config.notifier = Some(Arc::new(notify));
        self
    }

    /// A typed host handle over this tool's processes. Take it after the
    /// builders, since [`stats`](Self::stats) replaces the manager (and
    /// panics if a controller already exists).
    pub fn controller(&self) -> ProcessController {
        ProcessController::new(self)
    }

    fn core(&self) -> ProcessCore<'_> {
        ProcessCore {
            config: &self.config,
            manager: &self.manager,
        }
    }
}

/// Close the manager as soon as the tool goes: controllers report closed
/// at once, and their in-flight calls (a `wait_for_exit`, say) return
/// because the waiters kill the children when this token cancels, even
/// while such a call still holds the manager.
impl Drop for ProcessTool {
    fn drop(&mut self) {
        self.manager.shutdown.cancel();
    }
}

/// The one kill path, in two steps so a bulk kill can signal every
/// process before waiting on any: [`signal_kill`] then [`await_killed`].
/// The caller reports the termination itself (a `kill` result, a
/// session clear), so the autonomous exit wake is switched off: it
/// would say the same thing twice.
async fn terminate(proc: &Proc) {
    signal_kill(proc);
    await_killed(proc).await;
}

/// Silence the exit wake and tell the waiter to kill the child.
fn signal_kill(proc: &Proc) {
    proc.notify_on_exit.store(false, Ordering::Release);
    proc.kill.cancel();
}

/// Wait, bounded, for a signalled child's exit and output drain.
async fn await_killed(proc: &Proc) {
    let _ = tokio::time::timeout(Duration::from_secs(5), proc.done.cancelled()).await;
}

/// Between a child's exit and `done` (readers drained, waiter finished)
/// there is a window where a snapshot would pair "exited" with output
/// still in the pipes — the poll race that misreports a process's final
/// state. Callers about to snapshot an exited process wait the window
/// out; the waiter fires `done` within its ~200ms drain grace, so the
/// timeout is only a backstop against an aborted waiter.
async fn settle_exit(proc: &Proc) {
    if !proc.running() && !proc.done.is_cancelled() {
        let _ = tokio::time::timeout(Duration::from_secs(1), proc.done.cancelled()).await;
    }
}

fn required_str<'a>(input: &'a Value, key: &str, what: &str) -> Result<&'a str, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::msg(format!("`{key}` (string) is required for {what}")))
}

fn parse_spawn(input: &Value) -> Result<ProcessSpawn, ToolError> {
    let mut spawn = ProcessSpawn::new(required_str(input, "command", "spawn")?);
    spawn.wait_for_exit = input
        .get("waitForExit")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    spawn.notify_on_exit = input
        .get("notifyOnExit")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    spawn.notify_on_match = input
        .get("notifyOnMatch")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(spawn)
}

fn parse_write(input: &Value) -> Result<ProcessWrite, ToolError> {
    let mut write = ProcessWrite::new(required_str(input, "input", "write")?);
    write.newline = input
        .get("newline")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    write.eof = input.get("eof").and_then(Value::as_bool).unwrap_or(false);
    Ok(write)
}

#[async_trait]
impl Tool for ProcessTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process".into(),
            description: "Manage processes: `spawn` starts a command; set `waitForExit` for a \
                finite long-running command so its final result arrives in that same tool call \
                without polling. Leave it false for a server, watcher, or interactive REPL like \
                `python3 -i`; detached processes notify the host on exit by default, and \
                `notifyOnMatch` can wake it once when readiness or important output appears. \
                Use `poll` only for manual log checks, `write` for stdin, or `kill` to stop it. \
                `list` shows processes. Prefer `shell` for quick one-shot commands."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["spawn", "poll", "write", "kill", "list"]},
                    "command": {"type": "string", "description": "spawn: the command line to start."},
                    "waitForExit": {"type": "boolean", "default": false, "description": "spawn: wait for process exit and return its final result in this call, ignoring intermediate output. Use for finite long-running commands to avoid repeated polls."},
                    "notifyOnExit": {"type": "boolean", "default": true, "description": "spawn: for a detached process, notify the host once when it exits."},
                    "notifyOnMatch": {"type": "string", "description": "spawn: for a detached process, notify the host once when this literal text first appears in output. Use for server readiness or important log text."},
                    "id": {"type": "string", "description": "poll/write/kill: target process id."},
                    "input": {"type": "string", "description": "write: text to send to stdin."},
                    "newline": {"type": "boolean", "default": true, "description": "write: append a newline."},
                    "eof": {"type": "boolean", "default": false, "description": "write: close stdin afterwards."},
                    "waitMs": {"type": "integer", "description": "poll: how long to wait for new output (default 500, max 30000)."}
                },
                "required": ["action"]
            }),
        }
    }

    fn concurrency(&self, input: &Value) -> Concurrency {
        match input.get("action").and_then(Value::as_str) {
            Some("poll") | Some("write") | Some("kill") => {
                match input.get("id").and_then(Value::as_str) {
                    Some(id) => Concurrency::Keyed(format!("process:{id}")),
                    None => Concurrency::Serial,
                }
            }
            _ => Concurrency::Parallel,
        }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`action` (string) is required"))?;
        let core = self.core();
        let cancellation = &ctx.cancellation;
        let snapshot = match action {
            "spawn" => core.spawn(parse_spawn(&input)?, cancellation).await?,
            "poll" => {
                let id = required_str(&input, "id", "this action")?;
                let wait = input
                    .get("waitMs")
                    .and_then(Value::as_u64)
                    .map(Duration::from_millis);
                core.poll(id, wait, cancellation).await?
            }
            "write" => {
                let id = required_str(&input, "id", "this action")?;
                core.write(id, parse_write(&input)?, cancellation).await?
            }
            "kill" => core.kill(required_str(&input, "id", "kill")?).await?,
            "list" => return Ok(ProcessEntry::list_into_value(core.list())),
            other => {
                return Err(ToolError::msg(format!(
                    "unknown action `{other}`; expected spawn|poll|write|kill|list"
                )))
            }
        };
        Ok(snapshot.into_value())
    }
}
