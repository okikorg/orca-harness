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
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::Notify;

use orca_harness_core::{CancellationToken, Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use crate::pgroup;
use crate::shell::Executor;
use crate::BackgroundStats;

/// Merged, bounded, unread output of one process.
struct OutBuf {
    data: Vec<u8>,
    dropped: u64,
    cap: usize,
}

impl OutBuf {
    fn push(&mut self, chunk: &[u8]) {
        self.data.extend_from_slice(chunk);
        if self.data.len() > self.cap {
            let excess = self.data.len() - self.cap;
            self.data.drain(..excess);
            self.dropped += excess as u64;
        }
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
}

struct Proc {
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
}

impl Proc {
    fn running(&self) -> bool {
        self.exit.lock().unwrap().is_none()
    }
    fn exit_code(&self) -> Option<i32> {
        self.exit.lock().unwrap().flatten()
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

    fn snapshot(&self, id: &str, proc: &Proc) -> Value {
        let (output, dropped, more) = proc.buf.lock().unwrap().drain(self.max_output_bytes);
        let mut out = json!({
            "id": id,
            "output": output,
            "running": proc.running(),
            "exitCode": proc.exit_code(),
            "moreOutput": more,
        });
        if dropped > 0 {
            out["droppedBytes"] = json!(dropped);
        }
        out
    }

    async fn spawn(&self, input: &Value) -> Result<Value, ToolError> {
        let command_str = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`command` (string) is required for spawn"))?;

        if self.manager.procs.lock().unwrap().len() >= self.max_processes {
            return Err(ToolError::msg(format!(
                "live process limit reached ({}); kill one first",
                self.max_processes
            )));
        }

        let mut cmd = self
            .executor
            .build(command_str, self.working_dir.as_deref());
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::msg(format!("failed to spawn: {e}")))?;
        let pgid = child.id();

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();

        let id = format!("p{}", self.manager.seq.fetch_add(1, Ordering::SeqCst) + 1);
        let proc = Arc::new(Proc {
            command: command_str.to_string(),
            pgid,
            counted: AtomicBool::new(true),
            buf: Mutex::new(OutBuf {
                data: Vec::new(),
                dropped: 0,
                cap: self.buffer_cap,
            }),
            output_ready: Notify::new(),
            stdin: tokio::sync::Mutex::new(stdin),
            exit: Mutex::new(None),
            kill: self.manager.shutdown.child_token(),
            done: CancellationToken::new(),
        });

        let mut readers = Vec::new();
        for pipe in [stdout.map(either::Left), stderr.map(either::Right)] {
            let Some(pipe) = pipe else { continue };
            let p = proc.clone();
            readers.push(tokio::spawn(async move {
                let mut chunk = [0u8; 8192];
                match pipe {
                    either::Either::Left(mut out) => loop {
                        match out.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                p.buf.lock().unwrap().push(&chunk[..n]);
                                p.output_ready.notify_waiters();
                            }
                        }
                    },
                    either::Either::Right(mut err) => loop {
                        match err.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                p.buf.lock().unwrap().push(&chunk[..n]);
                                p.output_ready.notify_waiters();
                            }
                        }
                    },
                }
            }));
        }

        self.manager.stats.inc_processes();

        // The waiter owns the child: reap on exit or kill on demand, then
        // give the readers a moment to drain before signalling `done`.
        let p = proc.clone();
        let stats = self.manager.stats.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                biased;
                _ = p.kill.cancelled() => {
                    if let Some(pgid) = p.pgid {
                        pgroup::kill_group(pgid);
                    }
                    let _ = child.start_kill();
                    child.wait().await
                }
                status = child.wait() => status,
            };
            *p.exit.lock().unwrap() = Some(status.ok().and_then(|s| s.code()));
            if p.counted.swap(false, Ordering::Relaxed) {
                stats.dec_processes();
            }
            let _ = tokio::time::timeout(Duration::from_millis(200), async {
                for r in readers {
                    let _ = r.await;
                }
            })
            .await;
            p.done.cancel();
            p.output_ready.notify_waiters();
        });

        self.manager
            .procs
            .lock()
            .unwrap()
            .insert(id.clone(), proc.clone());

        // Give fast-failing commands a chance to report immediately.
        let _ = tokio::time::timeout(self.settle, proc.done.cancelled()).await;
        Ok(self.snapshot(&id, &proc))
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
        Ok(self.snapshot(&id, &proc))
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
        Ok(self.snapshot(&id, &proc))
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
        proc.kill.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), proc.done.cancelled()).await;
        Ok(self.snapshot(id, &proc))
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

/// Tiny stand-in for the `either` crate so both pipe types share one
/// reader loop without a dependency.
mod either {
    pub enum Either<L, R> {
        Left(L),
        Right(R),
    }
    pub use Either::{Left, Right};
}

#[async_trait]
impl Tool for ProcessTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process".into(),
            description: "Manage long-lived processes: `spawn` starts a command that keeps \
                running across calls (dev server, watcher, or an interactive REPL like \
                `python3 -i`), `poll` reads new output, `write` sends a line to its stdin, \
                `kill` stops it, `list` shows live processes. Prefer `shell` for quick \
                one-shot commands."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["spawn", "poll", "write", "kill", "list"]},
                    "command": {"type": "string", "description": "spawn: the command line to start."},
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
            "spawn" => self.spawn(&input).await,
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
