//! `bun_repl` — persistent JavaScript and TypeScript compute through Bun.
//!
//! Each call is written to a short-lived `.ts` file and loaded into one
//! long-running `bun repl`. `.load` preserves the REPL's lexical state and
//! native TypeScript/top-level-await semantics without feeding a large source
//! block through Bun's terminal line editor. Unique success/end sentinels turn
//! the interactive stream into a bounded request/response protocol.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use crate::pgroup;
use crate::BackgroundStats;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

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
    process: crate::Spawned,
    output: crate::spawner::Output,
}

#[derive(Default)]
struct Session {
    live: Option<Live>,
    restart_notice: bool,
    interrupted: Arc<AtomicBool>,
}

// The dispatcher may drop a tool future before its own cancellation branch
// runs. Mark the interpreter synchronously, and arrange owned async cleanup.
// A subsequent call checks the mark under the session lock before reuse.
struct InterruptedRepl {
    session: Arc<tokio::sync::Mutex<Session>>,
    interrupted: Arc<AtomicBool>,
    armed: bool,
}
impl Drop for InterruptedRepl {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.interrupted.store(true, Ordering::SeqCst);
        let session = self.session.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let mut session = session.lock().await;
                if session.interrupted.swap(false, Ordering::SeqCst) {
                    if let Some(live) = session.live.as_mut() {
                        live.process.kill().await;
                    }
                    session.live = None;
                    session.restart_notice = true;
                }
            });
        }
    }
}

/// Removes the per-call source even when execution is cancelled or times out.
///
/// The file has to live wherever `bun` does: `.load` is executed *by the
/// REPL*, so a host temp file is invisible to a sandboxed interpreter.
/// Under a sandbox the source is written through the provider and removed
/// the same way.
struct TempSource {
    path: PathBuf,
    /// Set when the file lives in a sandbox, so `Drop` knows where to
    /// delete it from.
    sandbox: Option<Arc<dyn orca_harness_core::Sandbox>>,
}

impl TempSource {
    /// The per-call file name. Process id plus sequence, so two tools in
    /// one process never collide and neither do two processes.
    fn file_name(seq: u64) -> String {
        format!("orca-bun-repl-{}-{seq}.ts", std::process::id())
    }

    fn body(code: &str, marker: &str) -> Vec<u8> {
        let mut out = code.as_bytes().to_vec();
        out.extend_from_slice(b"\n;console.log(");
        out.extend_from_slice(marker.as_bytes());
        out.extend_from_slice(b")\n");
        out
    }

    /// Write the source inside `sandbox`, in the directory `bun` runs in.
    async fn write_sandboxed(
        sandbox: &Arc<dyn orca_harness_core::Sandbox>,
        dir: &str,
        code: &str,
        marker: &str,
    ) -> Result<Self, ToolError> {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let path = PathBuf::from(dir).join(Self::file_name(seq));
        sandbox
            .write_file(
                &path.to_string_lossy(),
                &Self::body(code, marker),
                orca_harness_core::FileMode::Regular,
            )
            .await
            .map_err(|error: orca_harness_core::SandboxError| {
                ToolError::msg(format!("failed to write Bun REPL input: {error}"))
            })?;
        Ok(Self {
            path,
            sandbox: Some(sandbox.clone()),
        })
    }

    fn write(code: &str, marker: &str) -> Result<Self, ToolError> {
        let dir = std::env::temp_dir();
        if dir.to_string_lossy().chars().any(char::is_whitespace) {
            return Err(ToolError::msg(
                "bun_repl needs a temporary directory whose path has no whitespace",
            ));
        }
        for _ in 0..16 {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::SeqCst);
            let path = dir.join(Self::file_name(seq));
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
                    let source = Self {
                        path,
                        sandbox: None,
                    };
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
        let Some(sandbox) = self.sandbox.take() else {
            let _ = std::fs::remove_file(&self.path);
            return;
        };
        // A provider delete is async and `Drop` is not, so it is handed
        // to the runtime. The file is one small per-call source; losing
        // the race at shutdown leaks it inside a sandbox that is about to
        // be destroyed anyway.
        let path = std::mem::take(&mut self.path);
        tokio::spawn(async move {
            let command = format!("rm -f '{}'", path.to_string_lossy().replace('\'', r"'\''"));
            let _ = sandbox
                .exec(orca_harness_core::ExecRequest::new(command))
                .await;
        });
    }
}

pub struct BunReplTool {
    bun: String,
    /// Where the REPL runs. Local by default; a sandbox-backed spawner
    /// keeps the interpreter — and its per-call source file — inside the
    /// boundary with the rest of the agent's tools.
    spawner: crate::Spawner,
    working_dir: Option<String>,
    max_output_bytes: usize,
    default_timeout: Duration,
    max_timeout: Duration,
    session: Arc<tokio::sync::Mutex<Session>>,
    live_pgid: StdMutex<Option<u32>>,
    stats: BackgroundStats,
}

impl Default for BunReplTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BunReplTool {
    fn drop(&mut self) {
        let session = self.session.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let mut session = session.lock().await;
                if let Some(live) = session.live.as_mut() {
                    live.process.kill().await;
                }
                session.live = None;
            });
        }
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
            spawner: crate::Spawner::Local,
            working_dir: None,
            max_output_bytes: 64 * 1024,
            default_timeout: Duration::from_secs(30),
            max_timeout: Duration::from_secs(300),
            session: Arc::new(tokio::sync::Mutex::new(Session::default())),
            live_pgid: StdMutex::new(None),
            stats: BackgroundStats::default(),
        }
    }

    /// Run the interpreter inside `sandbox` instead of on this machine.
    /// The provider must report
    /// [`sessions`](orca_harness_core::Capabilities::sessions); one that
    /// does not cannot hold a live process, and the first call says so
    /// rather than silently starting one on the host.
    pub fn sandbox(mut self, sandbox: std::sync::Arc<dyn orca_harness_core::Sandbox>) -> Self {
        self.spawner = crate::Spawner::Sandbox(sandbox);
        self
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

    async fn spawn_repl(&self) -> Result<Live, ToolError> {
        let (process, output) = self
            .spawner
            .spawn(crate::Spawn {
                program: self.bun.clone(),
                args: vec!["repl".into(), "--no-install".into()],
                working_dir: self.working_dir.clone(),
                // Kept separate: the end-of-execution sentinel is written
                // to each stream and both must be seen.
                capture_stderr: true,
            })
            .await?;
        self.set_live_pgid(process.pgid());
        Ok(Live { process, output })
    }

    async fn kill_live(&self, session: &mut Session) {
        // Retain the handle if cancellation interrupts provider I/O, so a
        // later reset or SDK drop cleanup can retry the kill.
        if let Some(live) = session.live.as_mut() {
            live.process.kill().await;
        }
        session.live = None;
        self.set_live_pgid(None);
    }

    async fn exec(&self, input: &Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let rpc_enabled = ctx.programmatic_tools_enabled();
        if rpc_enabled && matches!(self.spawner, crate::Spawner::Local) {
            return Err(ToolError::msg("Programmatic Bun tools require a sandbox"));
        }
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
        if session.interrupted.swap(false, Ordering::SeqCst) {
            self.kill_live(&mut session).await;
            session.restart_notice = true;
        }
        let mut interrupted = InterruptedRepl {
            session: self.session.clone(),
            interrupted: session.interrupted.clone(),
            armed: true,
        };
        if let Some(live) = &mut session.live {
            if live.process.has_exited() {
                session.live = None;
                session.restart_notice = true;
                self.set_live_pgid(None);
            }
        }
        let restarted = session.restart_notice && session.live.is_none();
        if session.live.is_none() {
            session.live = Some(self.spawn_repl().await?);
            session.restart_notice = false;
        }

        let token = format!(
            "{}_{}_{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::SeqCst),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let ok = format!("ORCA_BUN_{token}_OK");
        let end = format!("\"ORCA_BUN_{token}_END\"");
        let stderr_end = format!("ORCA_BUN_{token}_STDERR_END");
        let marker = json!(ok).to_string();
        // The invocation capability is scoped to this execution, never stored
        // on the persistent interpreter or shared with a later run.
        let rpc_marker = format!("ORCA_BUN_{token}_RPC ");
        let rpc_path = format!(
            "{}/orca-bun-rpc-{token}.json",
            self.working_dir.as_deref().unwrap_or("/tmp")
        );
        let _rpc_cleanup = rpc_enabled.then(|| TempSource {
            path: PathBuf::from(&rpc_path),
            sandbox: match &self.spawner {
                crate::Spawner::Sandbox(s) => Some(s.clone()),
                _ => None,
            },
        });
        let bootstrap = if rpc_enabled {
            format!(
                r#"globalThis.tools = (() => {{
              const path = {path}; const marker = {marker}; let seq = 0; let queue = Promise.resolve();
              const request = (payload) => {{
                const run = async () => {{
                  const id = ++seq;
                  const frame = JSON.stringify({{id,...payload}});
                  if (Buffer.byteLength(frame) > 65536) throw new Error("Tool RPC request is too large");
                  process.stderr.write(marker + frame + "\n ");
                  for (;;) {{
                    try {{
                      const response = await Bun.file(path).json();
                      if (response.id === id) {{
                        if (response.error) throw new Error(response.error);
                        return response.result;
                      }}
                    }} catch (error) {{
                      if (error instanceof Error && error.message === "Programmatic dispatch failed") throw error;
                    }}
                    await Bun.sleep(5);
                  }}
                }};
                const result = queue.then(run); queue = result.catch(() => {{}}); return result;
              }};
              return Object.freeze({{
                batch: (calls) => request({{calls}}),
                call: async (name, arguments_) => {{
                  const [result] = await request({{calls:[{{name, arguments:arguments_}}]}});
                  if (result.is_error) throw new Error(JSON.stringify(result.output));
                  return result.output;
                }},
                list: () => request({{list:true}})
              }});
            }})();
"#,
                path = json!(rpc_path),
                marker = json!(rpc_marker)
            )
        } else {
            "globalThis.tools = undefined;\n".to_owned()
        };
        let rpc_code = format!("{bootstrap}\n{code}");
        let source = match &self.spawner {
            crate::Spawner::Sandbox(sandbox) => {
                // `.load` runs inside the REPL, so the file must exist
                // where the REPL can see it: the sandbox, not this host.
                TempSource::write_sandboxed(
                    sandbox,
                    self.working_dir.as_deref().unwrap_or("/tmp"),
                    &rpc_code,
                    &marker,
                )
                .await?
            }
            crate::Spawner::Local => TempSource::write(&rpc_code, &marker)?,
        };
        let end_command = format!(
            "process.stderr.write([\"ORCA\",\"BUN\",\"{token}\",\"STDERR\",\"END\"].join(\"_\") + \"\\n\"); [\"ORCA\",\"BUN\",\"{token}\",\"END\"].join(\"_\")\n"
        );
        let command = format!(".load {}\n{end_command}", source.path.display());
        let live = session.live.as_mut().expect("just ensured");
        if live.process.write_stdin(command.as_bytes()).await.is_err() {
            self.kill_live(&mut session).await;
            session.restart_notice = true;
            return Err(ToolError::msg(
                "Bun REPL stdin closed; it will restart on the next call",
            ));
        }

        let deadline = tokio::time::Instant::now() + timeout;
        let keep = self.max_output_bytes + ok.len() + end.len() + 4096;
        let stderr_keep = self.max_output_bytes + stderr_end.len() + 65536 + 256;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut dropped = 0u64;
        let mut ok_seen = false;
        let mut rpc_sequence = 0u64;
        let outcome = loop {
            if rpc_enabled {
                if let Some(start) = find(&stderr, rpc_marker.as_bytes()) {
                    if let Some(end) = stderr[start..].iter().position(|b| *b == b'\n') {
                        let frame = stderr[start + rpc_marker.len()..start + end].to_vec();
                        stderr.drain(start..=start + end);
                        let request: Value = serde_json::from_slice(&frame).unwrap_or(Value::Null);
                        let id = request["id"].as_u64().unwrap_or(0);
                        if frame.len() > 65536 || id != rpc_sequence + 1 {
                            break ReadOutcome::Closed;
                        }
                        rpc_sequence = id;
                        let operation = async {
                            let result = if request["list"] == true {
                                ctx.programmatic_tool_schemas().map(|v| json!(v))
                            } else {
                                match serde_json::from_value::<
                                    Vec<orca_harness_core::ProgrammaticCall>,
                                >(request["calls"].clone())
                                {
                                    Ok(calls) => ctx.dispatch_tools(calls).await.map(|v| json!(v)),
                                    Err(_) => Err(ToolError::msg("Invalid programmatic calls")),
                                }
                            };
                            let response = match result {
                                Ok(v) => json!({"id":id,"result":v}),
                                Err(_) => json!({"id":id,"error":"Programmatic dispatch failed"}),
                            };
                            let bytes = serde_json::to_vec(&response)
                                .map_err(|_| ToolError::msg("RPC serialization failed"))?;
                            if bytes.len() > 1024 * 1024 {
                                return Err(ToolError::msg("RPC result exceeds 1 MiB"));
                            }
                            match &self.spawner {
                                crate::Spawner::Sandbox(s) => s
                                    .write_file(
                                        &rpc_path,
                                        &bytes,
                                        orca_harness_core::FileMode::Regular,
                                    )
                                    .await
                                    .map_err(|_| ToolError::msg("RPC response failed")),
                                _ => unreachable!(),
                            }
                        };
                        tokio::select! {
                            biased;
                            _=ctx.cancellation.cancelled()=>break ReadOutcome::Cancelled,
                            _=tokio::time::sleep_until(deadline)=>break ReadOutcome::Timeout,
                            result=operation=>if result.is_err(){break ReadOutcome::Closed;}
                        }
                        continue;
                    }
                }
            }

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
                        // Look for the completion marker before trimming: a
                        // flood still draining behind it can push the marker
                        // out of the bounded buffer within this same chunk.
                        stdout.extend_from_slice(&chunk.bytes);
                        ok_seen |= find(&stdout, ok.as_bytes()).is_some();
                        dropped += trim_front(&mut stdout, keep);
                    }
                    None => break ReadOutcome::Closed,
                }
            }
        };

        if matches!(outcome, ReadOutcome::Complete { .. }) {
            interrupted.armed = false;
        }
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

    /// Request termination of the live interpreter and discard its state.
    /// Remote cleanup follows Spawner's best-effort kill semantics: it awaits
    /// the kill request but does not propagate provider errors or await exit.
    pub async fn reset(&self) -> Result<Value, ToolError> {
        let mut session = self.session.lock().await;
        self.kill_live(&mut session).await;
        session.restart_notice = false;
        Ok(json!({"state": "ok", "restarted": true, "output": "", "stderr": ""}))
    }
}

fn push_bounded(buffer: &mut Vec<u8>, chunk: &[u8], cap: usize) -> u64 {
    buffer.extend_from_slice(chunk);
    trim_front(buffer, cap)
}

fn trim_front(buffer: &mut Vec<u8>, cap: usize) -> u64 {
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
            description: "Execute JavaScript or TypeScript in a persistent Bun REPL. Variables, imports, and functions survive across calls; top-level await and multi-line code work. Use console.log() for output. When sandbox programmatic tools are enabled, await tools.list() for visible schemas, await tools.call(name, arguments) for an output, or await tools.batch([{name, arguments}]) for parallel call results. Tool errors reject tools.call; batch preserves is_error. Always await calls. reset discards all state. A timed-out execution kills the REPL, and the next call reports restarted: true."
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
