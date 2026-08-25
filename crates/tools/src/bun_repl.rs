//! `bun_repl` — persistent JavaScript and TypeScript compute through Bun.
//!
//! Each call is written to a short-lived `.ts` file and loaded into one
//! long-running `bun repl`. `.load` preserves the REPL's lexical state and
//! native TypeScript/top-level-await semantics without feeding a large source
//! block through Bun's terminal line editor. Unique success/end sentinels turn
//! the interactive stream into a bounded request/response protocol.

use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use crate::pgroup;
use crate::BackgroundStats;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

// Backpressure the child readers instead of letting a noisy or runaway
// program queue output without limit before `exec` can truncate it.
const OUTPUT_CHANNEL_CHUNKS: usize = 32;

struct OutputChunk {
    stderr: bool,
    bytes: Vec<u8>,
}

enum ReadOutcome {
    Complete {
        end_pos: usize,
        ok_pos: Option<usize>,
        ok_seen: bool,
        stderr_end_pos: usize,
    },
    Cancelled,
    Timeout,
    Closed,
}

struct Live {
    child: Child,
    stdin: ChildStdin,
    output: mpsc::Receiver<OutputChunk>,
    pgid: Option<u32>,
}

#[derive(Default)]
struct Session {
    live: Option<Live>,
    restart_notice: bool,
}

/// Removes the per-call source even when execution is cancelled or times out.
struct TempSource(PathBuf);

impl TempSource {
    fn write(code: &str, marker: &str) -> Result<Self, ToolError> {
        let dir = std::env::temp_dir();
        if dir.to_string_lossy().chars().any(char::is_whitespace) {
            return Err(ToolError::msg(
                "bun_repl needs a temporary directory whose path has no whitespace",
            ));
        }
        for _ in 0..16 {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::SeqCst);
            let path = dir.join(format!("orca-bun-repl-{}-{seq}.ts", std::process::id()));
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
                    let source = Self(path);
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
        let _ = std::fs::remove_file(&self.0);
    }
}

pub struct BunReplTool {
    bun: String,
    working_dir: Option<String>,
    max_output_bytes: usize,
    default_timeout: Duration,
    max_timeout: Duration,
    session: tokio::sync::Mutex<Session>,
    live_pgid: StdMutex<Option<u32>>,
    seq: AtomicU64,
    stats: BackgroundStats,
}

impl Default for BunReplTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BunReplTool {
    fn drop(&mut self) {
        if let Some(pgid) = self.live_pgid.lock().unwrap().take() {
            pgroup::kill_group(pgid);
            self.stats.dec_bun_repls();
        }
    }
}

impl BunReplTool {
    pub fn new() -> Self {
        Self {
            bun: "bun".into(),
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

    pub fn stats(mut self, stats: BackgroundStats) -> Self {
        self.stats = stats;
        self
    }

    pub fn bun(mut self, executable: impl Into<String>) -> Self {
        self.bun = executable.into();
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

    fn set_live_pgid(&self, pgid: Option<u32>) {
        let mut guard = self.live_pgid.lock().unwrap();
        match (guard.is_some(), pgid.is_some()) {
            (false, true) => self.stats.inc_bun_repls(),
            (true, false) => self.stats.dec_bun_repls(),
            _ => {}
        }
        *guard = pgid;
    }

    fn spawn_repl(&self) -> Result<Live, ToolError> {
        let mut command = tokio::process::Command::new(&self.bun);
        command
            .args(["repl", "--no-install"])
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = &self.working_dir {
            command.current_dir(dir);
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::msg(format!("failed to spawn {}: {error}", self.bun)))?;
        let pgid = child.id();
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let (send, output) = mpsc::channel(OUTPUT_CHANNEL_CHUNKS);
        pump_output(stdout, false, send.clone());
        pump_output(stderr, true, send);
        self.set_live_pgid(pgid);
        Ok(Live {
            child,
            stdin,
            output,
            pgid,
        })
    }

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
        if let Some(live) = &mut session.live {
            if live.child.try_wait().ok().flatten().is_some() {
                session.live = None;
                session.restart_notice = true;
                self.set_live_pgid(None);
            }
        }
        let restarted = session.restart_notice && session.live.is_none();
        if session.live.is_none() {
            session.live = Some(self.spawn_repl()?);
            session.restart_notice = false;
        }

        let token = format!(
            "{}_{}",
            std::process::id(),
            self.seq.fetch_add(1, Ordering::SeqCst)
        );
        let ok = format!("ORCA_BUN_{token}_OK");
        let end = format!("\"ORCA_BUN_{token}_END\"");
        let stderr_end = format!("ORCA_BUN_{token}_STDERR_END");
        let source = TempSource::write(code, &json!(ok).to_string())?;
        let end_command = format!(
            "process.stderr.write([\"ORCA\",\"BUN\",\"{token}\",\"STDERR\",\"END\"].join(\"_\") + \"\\n\"); [\"ORCA\",\"BUN\",\"{token}\",\"END\"].join(\"_\")\n"
        );
        let command = format!(".load {}\n{end_command}", source.0.display());
        let live = session.live.as_mut().expect("just ensured");
        if live.stdin.write_all(command.as_bytes()).await.is_err()
            || live.stdin.flush().await.is_err()
        {
            self.kill_live(&mut session).await;
            session.restart_notice = true;
            return Err(ToolError::msg(
                "Bun REPL stdin closed; it will restart on the next call",
            ));
        }

        let deadline = tokio::time::Instant::now() + timeout;
        let keep = self.max_output_bytes + ok.len() + end.len() + 4096;
        let stderr_keep = self.max_output_bytes + stderr_end.len() + 256;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut dropped = 0u64;
        let mut ok_seen = false;
        let outcome = loop {
            if let (Some(end_pos), Some(stderr_end_pos)) = (
                find(&stdout, end.as_bytes()),
                find(&stderr, stderr_end.as_bytes()),
            ) {
                let ok_pos = find(&stdout[..end_pos], ok.as_bytes());
                break ReadOutcome::Complete {
                    end_pos,
                    ok_pos,
                    ok_seen: ok_seen || ok_pos.is_some(),
                    stderr_end_pos,
                };
            }
            tokio::select! {
                biased;
                _ = ctx.cancellation.cancelled() => break ReadOutcome::Cancelled,
                _ = tokio::time::sleep_until(deadline) => break ReadOutcome::Timeout,
                chunk = live.output.recv() => match chunk {
                    Some(chunk) if chunk.stderr => {
                        dropped += push_bounded(&mut stderr, &chunk.bytes, stderr_keep);
                    }
                    Some(chunk) => {
                        dropped += push_bounded(&mut stdout, &chunk.bytes, keep);
                        ok_seen |= find(&stdout, ok.as_bytes()).is_some();
                    }
                    None => break ReadOutcome::Closed,
                }
            }
        };

        match outcome {
            ReadOutcome::Complete {
                end_pos,
                ok_pos,
                ok_seen,
                stderr_end_pos,
            } => {
                let capture_end = ok_pos.unwrap_or(end_pos);
                let mut output = clean_stdout(&stdout[..capture_end]);
                let mut stderr = clean_text(&stderr[..stderr_end_pos]);
                dropped += truncate_tail(&mut output, self.max_output_bytes);
                dropped += truncate_tail(&mut stderr, self.max_output_bytes);
                let mut result = json!({
                    "state": if ok_seen { "ok" } else { "error" },
                    "output": output,
                    "stderr": stderr,
                });
                if restarted {
                    result["restarted"] = json!(true);
                }
                if dropped > 0 {
                    result["droppedBytes"] = json!(dropped);
                }
                Ok(result)
            }
            ReadOutcome::Cancelled => {
                self.kill_live(&mut session).await;
                session.restart_notice = true;
                Err(ToolError::msg(
                    "cancelled; Bun REPL will restart on the next call",
                ))
            }
            ReadOutcome::Timeout => {
                self.kill_live(&mut session).await;
                session.restart_notice = true;
                let mut output = clean_stdout(&stdout);
                let mut stderr = clean_text(&stderr);
                dropped += truncate_tail(&mut output, self.max_output_bytes);
                dropped += truncate_tail(&mut stderr, self.max_output_bytes);
                let mut result = json!({
                    "state": "timeout",
                    "output": output,
                    "stderr": stderr,
                    "error": format!(
                        "execution exceeded {}ms; the Bun REPL was killed and will restart (state lost) on the next call",
                        timeout.as_millis()
                    ),
                });
                if dropped > 0 {
                    result["droppedBytes"] = json!(dropped);
                }
                Ok(result)
            }
            ReadOutcome::Closed => {
                self.kill_live(&mut session).await;
                session.restart_notice = true;
                Err(ToolError::msg(
                    "Bun REPL exited before completing; it will restart on the next call",
                ))
            }
        }
    }

    async fn reset(&self) -> Result<Value, ToolError> {
        let mut session = self.session.lock().await;
        self.kill_live(&mut session).await;
        session.restart_notice = false;
        Ok(json!({"state": "ok", "restarted": true, "output": "", "stderr": ""}))
    }
}

fn pump_output<R>(mut reader: R, stderr: bool, send: mpsc::Sender<OutputChunk>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if send
                        .send(OutputChunk {
                            stderr,
                            bytes: buffer[..read].to_vec(),
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

fn push_bounded(buffer: &mut Vec<u8>, chunk: &[u8], cap: usize) -> u64 {
    buffer.extend_from_slice(chunk);
    if buffer.len() <= cap {
        return 0;
    }
    let excess = buffer.len() - cap;
    buffer.drain(..excess);
    excess as u64
}

fn truncate_tail(text: &mut String, max_bytes: usize) -> u64 {
    if text.len() <= max_bytes {
        return 0;
    }
    let mut cut = text.len() - max_bytes;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    text.drain(..cut);
    cut as u64
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Bun's line editor redraws piped input with CR + CSI 2K. Those rows are
/// input echo, not program output, so discard them along with the banner and
/// `.load` notice. User output is otherwise preserved as plain text.
fn clean_stdout(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .split_inclusive('\n')
        .filter(|line| !line.contains("\u{1b}[2K"))
        .map(strip_ansi)
        .filter(|line| {
            let line = line.trim();
            !line.starts_with("Welcome to Bun")
                && !line.starts_with("Type .copy")
                && !line.starts_with("Loading ")
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn clean_text(raw: &[u8]) -> String {
    strip_ansi(&String::from_utf8_lossy(raw)).trim().to_owned()
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        } else if ch != '\r' {
            output.push(ch);
        }
    }
    output
}

#[async_trait]
impl Tool for BunReplTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "bun_repl".into(),
            description: "Execute JavaScript or TypeScript in a persistent Bun REPL. Variables, imports, and functions survive across calls; top-level await and multi-line code work. Use console.log() for output. reset discards all state. A timed-out execution kills the REPL, and the next call reports restarted: true."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "JavaScript or TypeScript source to execute (required for exec)."},
                    "action": {"type": "string", "enum": ["exec", "reset"], "default": "exec"},
                    "timeoutMs": {"type": "integer", "description": "Per-call execution cap in milliseconds (default 30000)."}
                }
            }),
        }
    }

    fn concurrency(&self, _input: &Value) -> Concurrency {
        Concurrency::Keyed("bun_repl".into())
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
mod tests;
