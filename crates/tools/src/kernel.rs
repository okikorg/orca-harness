//! `kernel` — stateful Python compute.
//!
//! Where `process` can drive `python3 -i` line by line, the interactive
//! REPL wedges on multi-line input. The kernel never talks to the REPL:
//! it spawns `python3 -u -c '<driver>' <nonce>` where the driver reads
//! byte-count-framed code blocks from stdin, runs each with `exec`
//! against one persistent globals dict, and delimits every execution's
//! output with a per-spawn sentinel line. Arbitrary multi-line code runs
//! verbatim; variables persist across calls.
//!
//! Recovery: a timed-out or cancelled execution leaves the driver in an
//! unknown spot, so the kernel is killed (whole process group) and the
//! next call respawns fresh, reporting `restarted: true` — state loss is
//! explicit, never silent. The driver also exits on stdin EOF, so a dead
//! host cannot leave a kernel behind.

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout};

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use crate::pgroup;
use crate::BackgroundStats;

/// The in-process side of the framing protocol. `os.dup2(1, 2)` merges
/// stderr into stdout at the fd level so ordering is preserved even for
/// subprocesses the user's code spawns.
const DRIVER: &str = r#"
import sys, os, traceback
os.dup2(1, 2)
g = {'__name__': '__main__'}
n = sys.argv[1]
while True:
    h = sys.stdin.buffer.readline()
    if not h:
        break
    h = h.strip()
    if not h.startswith(b'EXEC '):
        continue
    k = int(h[5:])
    b = sys.stdin.buffer.read(k)
    while len(b) < k:
        m = sys.stdin.buffer.read(k - len(b))
        if not m:
            break
        b += m
    try:
        exec(compile(b.decode('utf-8'), '<kernel>', 'exec'), g)
        sys.stdout.flush()
        sys.stdout.write('\n%s ok\n' % n)
    except BaseException:
        sys.stdout.flush()
        sys.stdout.write('\n%s err\n' % n)
        sys.stdout.write(traceback.format_exc())
        sys.stdout.write('%s end\n' % n)
    sys.stdout.flush()
"#;

struct Live {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    nonce: String,
    pgid: Option<u32>,
}

#[derive(Default)]
struct Session {
    live: Option<Live>,
    /// The previous kernel died losing state (wedge, crash, cancel); the
    /// next successful spawn reports `restarted: true`.
    restart_notice: bool,
}

pub struct KernelTool {
    python: String,
    working_dir: Option<String>,
    max_output_bytes: usize,
    default_timeout: Duration,
    max_timeout: Duration,
    session: tokio::sync::Mutex<Session>,
    /// Mirror of the live kernel's pgid, readable from the sync `Drop`.
    live_pgid: StdMutex<Option<u32>>,
    seq: AtomicU64,
    stats: BackgroundStats,
}

impl Default for KernelTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for KernelTool {
    fn drop(&mut self) {
        let pgid = self.live_pgid.lock().unwrap().take();
        if let Some(pgid) = pgid {
            pgroup::kill_group(pgid);
            self.stats.dec_kernels();
        }
    }
}

impl KernelTool {
    pub fn new() -> Self {
        Self {
            python: "python3".into(),
            working_dir: None,
            max_output_bytes: 64 * 1024,
            default_timeout: Duration::from_secs(30),
            max_timeout: Duration::from_secs(300),
            session: tokio::sync::Mutex::new(Session::default()),
            live_pgid: StdMutex::new(None),
            seq: AtomicU64::new(0),
            stats: BackgroundStats::default(),
        }
    }

    /// Adopt shared live counters.
    pub fn stats(mut self, stats: BackgroundStats) -> Self {
        self.stats = stats;
        self
    }

    /// The single place the live-pgid mirror changes; keeps the kernels
    /// counter exactly in step with it.
    fn set_live_pgid(&self, pgid: Option<u32>) {
        let mut guard = self.live_pgid.lock().unwrap();
        match (guard.is_some(), pgid.is_some()) {
            (false, true) => self.stats.inc_kernels(),
            (true, false) => self.stats.dec_kernels(),
            _ => {}
        }
        *guard = pgid;
    }

    pub fn python(mut self, interpreter: impl Into<String>) -> Self {
        self.python = interpreter.into();
        self
    }

    pub fn working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    pub fn max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    pub fn default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    fn spawn_kernel(&self) -> Result<Live, ToolError> {
        let nonce = format!(
            "ORCA_K_{}_{}",
            std::process::id(),
            self.seq.fetch_add(1, Ordering::SeqCst)
        );
        let mut cmd = tokio::process::Command::new(&self.python);
        cmd.args(["-u", "-c", DRIVER, &nonce])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(dir) = &self.working_dir {
            cmd.current_dir(dir);
        }
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::msg(format!("failed to spawn {}: {e}", self.python)))?;
        let pgid = child.id();
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        self.set_live_pgid(pgid);
        Ok(Live {
            child,
            stdin,
            stdout,
            nonce,
            pgid,
        })
    }

    /// Kill the live kernel (whole group) and reap it.
    async fn kill_live(&self, session: &mut Session) {
        if let Some(mut live) = session.live.take() {
            if let Some(pgid) = live.pgid {
                pgroup::kill_group(pgid);
            }
            let _ = live.child.start_kill();
            let _ = live.child.wait().await;
        }
        self.set_live_pgid(None);
    }

    async fn exec(&self, input: &Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let code = input
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`code` (string) is required for exec"))?;
        let timeout = input
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(self.default_timeout)
            .min(self.max_timeout);

        let mut session = self.session.lock().await;

        // A kernel that exited on its own (crash, os._exit) lost state too.
        if let Some(live) = &mut session.live {
            if live.child.try_wait().ok().flatten().is_some() {
                session.live = None;
                session.restart_notice = true;
                self.set_live_pgid(None);
            }
        }
        let restarted = session.restart_notice && session.live.is_none();
        if session.live.is_none() {
            session.live = Some(self.spawn_kernel()?);
            session.restart_notice = false;
        }
        let live = session.live.as_mut().expect("just ensured");

        let header = format!("EXEC {}\n", code.len());
        let write = async {
            live.stdin.write_all(header.as_bytes()).await?;
            live.stdin.write_all(code.as_bytes()).await?;
            live.stdin.flush().await
        };
        if write.await.is_err() {
            // Broken pipe: kernel died mid-write. Recover next call.
            self.kill_live(&mut session).await;
            session.restart_notice = true;
            return Err(ToolError::msg(
                "kernel stdin closed; it will restart on the next call",
            ));
        }

        // Read until the sentinel, bounded by timeout and cancellation.
        let ok_mark = format!("\n{} ok\n", live.nonce).into_bytes();
        let err_mark = format!("\n{} err\n", live.nonce).into_bytes();
        let end_mark = format!("{} end\n", live.nonce).into_bytes();
        let mut buf: Vec<u8> = Vec::new();
        let mut dropped: u64 = 0;
        // Retain enough to always find a sentinel that straddles reads.
        let keep = self.max_output_bytes + ok_mark.len().max(err_mark.len()) + end_mark.len() + 256;

        let outcome = loop {
            if let Some(pos) = find(&buf, &ok_mark) {
                break Some(("ok", pos, None));
            }
            if let Some(pos) = find(&buf, &err_mark) {
                let tb_start = pos + err_mark.len();
                if let Some(end) = find(&buf[tb_start..], &end_mark) {
                    let traceback =
                        String::from_utf8_lossy(&buf[tb_start..tb_start + end]).into_owned();
                    break Some(("error", pos, Some(traceback)));
                }
            }
            let mut chunk = [0u8; 8192];
            let read = tokio::select! {
                biased;
                _ = ctx.cancellation.cancelled() => break None,
                read = tokio::time::timeout(timeout, live.stdout.read(&mut chunk)) => read,
            };
            match read {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break None, // EOF, error, or timeout
                Ok(Ok(n)) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > keep {
                        let excess = buf.len() - keep;
                        buf.drain(..excess);
                        dropped += excess as u64;
                    }
                }
            }
        };

        match outcome {
            Some((state, pos, traceback)) => {
                let mut output = &buf[..pos];
                // `pos` marks the '\n' the driver writes before the
                // sentinel to force a line boundary, so the seam itself is
                // already excluded from `output`.
                if output.len() > self.max_output_bytes {
                    let excess = output.len() - self.max_output_bytes;
                    dropped += excess as u64;
                    output = &output[excess..];
                }
                let mut out = json!({
                    "state": state,
                    "output": String::from_utf8_lossy(output).into_owned(),
                });
                if let Some(tb) = traceback {
                    out["traceback"] = json!(tb);
                }
                if restarted {
                    out["restarted"] = json!(true);
                }
                if dropped > 0 {
                    out["droppedBytes"] = json!(dropped);
                }
                Ok(out)
            }
            None => {
                // Timeout, cancellation, or a dead driver: the framing can
                // no longer be trusted. Kill now, respawn on the next call.
                let cancelled = ctx.cancellation.is_cancelled();
                self.kill_live(&mut session).await;
                session.restart_notice = true;
                if cancelled {
                    return Err(ToolError::msg(
                        "cancelled; kernel will restart on the next call",
                    ));
                }
                let mut output = &buf[..];
                if output.len() > self.max_output_bytes {
                    output = &output[output.len() - self.max_output_bytes..];
                }
                Ok(json!({
                    "state": "timeout",
                    "output": String::from_utf8_lossy(output).into_owned(),
                    "error": format!(
                        "execution exceeded {}ms; the kernel was killed and will restart (state lost) on the next call",
                        timeout.as_millis()
                    ),
                }))
            }
        }
    }

    async fn reset(&self) -> Result<Value, ToolError> {
        let mut session = self.session.lock().await;
        self.kill_live(&mut session).await;
        session.restart_notice = false; // explicit reset: the model asked
        Ok(json!({"state": "ok", "restarted": true, "output": ""}))
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[async_trait]
impl Tool for KernelTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "kernel".into(),
            description: "Execute Python in a persistent kernel: variables, imports, and \
                functions survive across calls, so build state incrementally and reuse it. \
                Multi-line code is fine. Nothing is auto-echoed — `print()` what you want \
                to see. `reset` discards all state. A timed-out execution kills the kernel; \
                the next call starts fresh and reports `restarted: true`."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "Python source to execute (required for exec)."},
                    "action": {"type": "string", "enum": ["exec", "reset"], "default": "exec"},
                    "timeoutMs": {"type": "integer", "description": "Per-call execution cap in milliseconds (default 30000)."}
                }
            }),
        }
    }

    /// One kernel, one stdin: calls serialize in call order.
    fn concurrency(&self, _input: &Value) -> Concurrency {
        Concurrency::Serial
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        match input.get("action").and_then(Value::as_str) {
            None | Some("exec") => self.exec(&input, ctx).await,
            Some("reset") => self.reset().await,
            Some(other) => Err(ToolError::msg(format!(
                "unknown action `{other}`; expected exec|reset"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DRIVER;
    use std::process::Stdio;

    /// The no-dangling backstop: a driver whose stdin closes must exit on
    /// its own, even if nobody kills it.
    #[tokio::test]
    async fn driver_exits_on_stdin_eof() {
        let probe = std::process::Command::new("python3").arg("--version").output();
        if !probe.is_ok_and(|o| o.status.success()) {
            eprintln!("skipping: python3 not found on PATH");
            return;
        }
        let mut child = tokio::process::Command::new("python3")
            .args(["-u", "-c", DRIVER, "NONCE"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        drop(child.stdin.take()); // EOF
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .expect("driver must exit on stdin EOF")
            .unwrap();
        assert!(status.success());
    }
}
