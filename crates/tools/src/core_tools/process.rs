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
//! Lifetime: children are killed when the tool (and thus its manager) is
//! dropped, and a run's cancellation only interrupts the current call —
//! processes deliberately survive between calls. There is no PTY here;
//! programs that refuse to run without one need the host to provide a
//! richer executor.

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

mod spawn;

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

struct MatchState {
    pattern: String,
    needle: Vec<u8>,
    tail: Vec<u8>,
    armed: bool,
    notified: bool,
}

impl MatchState {
    fn new(pattern: String) -> Self {
        Self {
            needle: pattern.as_bytes().to_vec(),
            pattern,
            tail: Vec::new(),
            armed: false,
            notified: false,
        }
    }

    fn observe(&mut self, chunk: &[u8]) -> bool {
        if !self.armed || self.notified {
            return false;
        }
        let mut window = Vec::with_capacity(self.tail.len() + chunk.len());
        window.extend_from_slice(&self.tail);
        window.extend_from_slice(chunk);
        if window
            .windows(self.needle.len())
            .any(|candidate| candidate == self.needle)
        {
            self.notified = true;
            return true;
        }
        let keep = self.needle.len().saturating_sub(1).min(window.len());
        self.tail.clear();
        self.tail.extend_from_slice(&window[window.len() - keep..]);
        false
    }

    fn arm(&mut self) {
        self.tail.clear();
        self.armed = true;
    }
}

/// Merged, bounded, unread output of one process.
struct OutBuf {
    data: Vec<u8>,
    dropped: u64,
    cap: usize,
    notification_data: Vec<u8>,
    notification_dropped: u64,
    notification_cap: usize,
    notify_match: Option<MatchState>,
}

impl OutBuf {
    fn push(&mut self, chunk: &[u8]) -> Option<String> {
        let matched = self
            .notify_match
            .as_mut()
            .and_then(|state| state.observe(chunk).then(|| state.pattern.clone()));
        self.data.extend_from_slice(chunk);
        if self.data.len() > self.cap {
            let excess = self.data.len() - self.cap;
            self.data.drain(..excess);
            self.dropped += excess as u64;
        }
        if self.notification_cap > 0 {
            self.notification_data.extend_from_slice(chunk);
            if self.notification_data.len() > self.notification_cap {
                let excess = self.notification_data.len() - self.notification_cap;
                self.notification_data.drain(..excess);
                self.notification_dropped += excess as u64;
            }
        }
        matched
    }

    /// Take up to `max` bytes. Cuts may split a UTF-8 sequence; the lossy
    /// conversion degrades that to a replacement char at the seam only.
    fn drain(&mut self, max: usize) -> (String, u64, bool) {
        let dropped = std::mem::take(&mut self.dropped);
        let take = self.data.len().min(max);
        let chunk: Vec<u8> = self.data.drain(..take).collect();
        let more = !self.data.is_empty();
        (String::from_utf8_lossy(&chunk).into_owned(), dropped, more)
    }

    fn drain_notification(&mut self) -> (String, u64) {
        let output = String::from_utf8_lossy(&std::mem::take(&mut self.notification_data)).into();
        let dropped = std::mem::take(&mut self.notification_dropped);
        (output, dropped)
    }

    fn arm_notifications(&mut self) {
        self.notification_data.clear();
        self.notification_dropped = 0;
        if let Some(state) = &mut self.notify_match {
            state.arm();
        }
    }
}

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
                self.stats.dec_processes();
            }
        }
    }
}

pub struct ProcessTool {
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
    manager: Arc<Manager>,
    notifier: Option<ProcessNotifier>,
}

impl Default for ProcessTool {
    fn default() -> Self {
        Self::new(Executor::local_sh())
    }
}

impl ProcessTool {
    pub fn new(executor: Executor) -> Self {
        Self {
            executor,
            working_dir: None,
            max_output_bytes: 64 * 1024,
            buffer_cap: 512 * 1024,
            settle: Duration::from_millis(500),
            max_wait: Duration::from_secs(30),
            max_processes: 32,
            manager: Arc::new(Manager {
                seq: AtomicU64::new(0),
                procs: Mutex::new(HashMap::new()),
                shutdown: CancellationToken::new(),
                stats: BackgroundStats::default(),
            }),
            notifier: None,
        }
    }

    /// Adopt shared live counters. Call before any spawn.
    pub fn stats(mut self, stats: BackgroundStats) -> Self {
        self.manager = Arc::new(Manager {
            seq: AtomicU64::new(0),
            procs: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
            stats,
        });
        self
    }

    /// Convenience: host-local processes.
    pub fn local() -> Self {
        Self::default()
    }

    pub fn working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    pub fn max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    pub fn max_processes(mut self, n: usize) -> Self {
        self.max_processes = n;
        self
    }

    pub fn on_notification(
        mut self,
        notify: impl Fn(ProcessNotification) + Send + Sync + 'static,
    ) -> Self {
        self.notifier = Some(Arc::new(notify));
        self
    }

    fn get(&self, input: &Value) -> Result<(String, Arc<Proc>), ToolError> {
        let id = input
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`id` (string) is required for this action"))?;
        let proc = self
            .manager
            .procs
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| ToolError::msg(format!("unknown process id: {id}")))?;
        Ok((id.to_string(), proc))
    }

    fn snapshot(&self, id: &str, proc: &Proc, arm_exit: bool, arm_match: bool) -> Value {
        // Read the exit slot once, and before draining: `running` and
        // `exitCode` must describe the same instant, and reading liveness
        // first means an exit racing this snapshot shows up as "still
        // running, output partial" (the next poll completes the story)
        // rather than "exited" with output still in flight.
        let exit_guard = proc.exit.lock().unwrap();
        let exit = *exit_guard;
        let mut buf = proc.buf.lock().unwrap();
        let (output, dropped, more) = buf.drain(self.max_output_bytes);
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
        let mut out = json!({
            "id": id,
            "output": output,
            "running": exit.is_none(),
            "exitCode": exit.flatten(),
            "moreOutput": more,
        });
        if dropped > 0 {
            out["droppedBytes"] = json!(dropped);
        }
        out
    }

    async fn poll(&self, input: &Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let (id, proc) = self.get(input)?;
        let wait = input
            .get("waitMs")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(self.settle)
            .min(self.max_wait);

        let notified = proc.output_ready.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let idle = proc.buf.lock().unwrap().data.is_empty();
        if idle && proc.running() {
            tokio::select! {
                biased;
                _ = ctx.cancellation.cancelled() => return Err(ToolError::msg("cancelled")),
                _ = proc.done.cancelled() => {}
                _ = &mut notified => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
        settle_exit(&proc).await;
        Ok(self.snapshot(&id, &proc, false, false))
    }

    async fn write(&self, input: &Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let (id, proc) = self.get(input)?;
        let text = input
            .get("input")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`input` (string) is required for write"))?;
        let newline = input
            .get("newline")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let eof = input.get("eof").and_then(Value::as_bool).unwrap_or(false);

        if !proc.running() {
            return Err(ToolError::msg(format!("process {id} has exited")));
        }
        {
            let mut guard = proc.stdin.lock().await;
            let stdin = guard
                .as_mut()
                .ok_or_else(|| ToolError::msg(format!("stdin of {id} is closed")))?;
            let data = if newline {
                format!("{text}\n")
            } else {
                text.to_string()
            };
            stdin
                .write_all(data.as_bytes())
                .await
                .map_err(|e| ToolError::msg(format!("write to stdin failed: {e}")))?;
            stdin
                .flush()
                .await
                .map_err(|e| ToolError::msg(format!("flush stdin failed: {e}")))?;
            if eof {
                *guard = None; // drop the handle → child sees EOF
            }
        }

        tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => return Err(ToolError::msg("cancelled")),
            _ = proc.done.cancelled() => {}
            _ = tokio::time::sleep(self.settle) => {}
        }
        settle_exit(&proc).await;
        Ok(self.snapshot(&id, &proc, false, false))
    }

    async fn kill(&self, input: &Value) -> Result<Value, ToolError> {
        let id = input
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`id` (string) is required for kill"))?;
        let proc = self
            .manager
            .procs
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(|| ToolError::msg(format!("unknown process id: {id}")))?;
        // The caller receives this termination through the `kill` result;
        // an autonomous exit wake would tell the model the same thing twice.
        proc.notify_on_exit.store(false, Ordering::Release);
        proc.kill.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), proc.done.cancelled()).await;
        Ok(self.snapshot(id, &proc, false, false))
    }

    fn list(&self) -> Value {
        let procs = self.manager.procs.lock().unwrap();
        let mut entries: Vec<Value> = procs
            .iter()
            .map(|(id, p)| {
                json!({
                    "id": id,
                    "command": p.command.chars().take(200).collect::<String>(),
                    "running": p.running(),
                    "exitCode": p.exit_code(),
                })
            })
            .collect();
        entries.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        json!({ "processes": entries })
    }
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
        match action {
            "spawn" => self.spawn(&input, ctx).await,
            "poll" => self.poll(&input, ctx).await,
            "write" => self.write(&input, ctx).await,
            "kill" => self.kill(&input).await,
            "list" => Ok(self.list()),
            other => Err(ToolError::msg(format!(
                "unknown action `{other}`; expected spawn|poll|write|kill|list"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MatchState;

    #[test]
    fn output_match_spans_chunks_and_notifies_once() {
        let mut state = MatchState::new("server ready".into());
        state.arm();

        assert!(!state.observe(b"server rea"));
        assert!(state.observe(b"dy on :3000"));
        assert!(!state.observe(b" server ready again"));
    }

    #[test]
    fn output_match_ignores_everything_before_it_is_armed() {
        let mut state = MatchState::new("ready".into());

        assert!(!state.observe(b"ready"));
        state.arm();
        assert!(!state.observe(b"ady"), "pre-arm tail is discarded");
        assert!(state.observe(b"ready"));
    }
}
