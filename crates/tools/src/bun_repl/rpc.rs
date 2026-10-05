//! Programmatic tool calls from Bun code: the `tools` global and the
//! request/response protocol behind it.
//!
//! Requests travel as tagged, numbered frames on the interpreter's
//! stderr; each response is written to a per-execution file that the Bun
//! side polls. Both live wherever `bun` does, so a sandboxed interpreter
//! needs no listener and no route back to the host. The Bun side queues
//! its requests, so one is in flight at a time.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use super::{find, BunReplTool, Session, TempSource, TEMP_SEQ};
use crate::ToolDispatch;

/// Largest request frame, in bytes, either side accepts.
pub(super) const MAX_FRAME: usize = 64 * 1024;
/// Largest serialized response.
const MAX_RESPONSE: usize = 1024 * 1024;
/// Most calls in one `tools.batch`.
const MAX_BATCH: usize = 64;

/// Removes the global a programmatic execution installed, so a later
/// execution without the capability cannot reach a stale one.
pub(super) const UNINSTALL: &str = "delete globalThis.tools;\n";

/// `bun_repl` with the capability to call the run's other tools. Built
/// per run by [`BunReplTool::with_dispatch`]; the persistent interpreter
/// never stores the handle.
struct DispatchingBunRepl {
    repl: Arc<BunReplTool>,
    dispatch: ToolDispatch,
}

impl BunReplTool {
    /// This interpreter as a tool whose code can call the tools in
    /// `dispatch` through `await tools.list()`, `await tools.call(name,
    /// arguments)` and `await tools.batch([{name, arguments}])`. Register
    /// the result in place of the plain tool for one run. It schedules as
    /// [`Concurrency::Serial`]: nested calls run while no sibling of the
    /// parent call does, so a nested `Serial` call stays exclusive.
    pub fn with_dispatch(self: Arc<Self>, dispatch: ToolDispatch) -> Arc<dyn Tool> {
        Arc::new(DispatchingBunRepl {
            repl: self,
            dispatch,
        })
    }
}

#[async_trait]
impl Tool for DispatchingBunRepl {
    fn schema(&self) -> ToolSchema {
        let mut schema = self.repl.schema();
        schema.description.push_str(
            " Code can call the agent's other tools: await tools.list() returns their schemas, await tools.call(name, arguments) returns a tool's output and throws on a tool error, and await tools.batch([{name, arguments}]) runs up to 64 calls in parallel and returns every result with its is_error flag. Always await these calls.",
        );
        schema
    }

    fn concurrency(&self, _input: &Value) -> Concurrency {
        Concurrency::Serial
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        self.repl.call_with(input, ctx, Some(&self.dispatch)).await
    }
}

/// One execution's end of the protocol.
pub(super) struct Rpc<'a> {
    dispatch: &'a ToolDispatch,
    marker: String,
    /// The response file, where `bun` runs; removed on drop.
    file: TempSource,
    sequence: u64,
}

#[derive(Deserialize)]
struct Call {
    name: String,
    #[serde(default = "empty_arguments")]
    arguments: Value,
}

fn empty_arguments() -> Value {
    json!({})
}

impl<'a> Rpc<'a> {
    pub(super) fn new(dispatch: &'a ToolDispatch, repl: &BunReplTool) -> Self {
        let id = format!(
            "{}_{}_{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::SeqCst),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let name = format!("orca-bun-rpc-{id}.json");
        let (path, sandbox) = match &repl.spawner {
            crate::Spawner::Sandbox(sandbox) => (
                PathBuf::from(repl.working_dir.as_deref().unwrap_or("/tmp")).join(name),
                Some(sandbox.clone()),
            ),
            crate::Spawner::Local => (std::env::temp_dir().join(name), None),
        };
        Self {
            dispatch,
            marker: format!("ORCA_BUN_RPC_{id} "),
            file: TempSource { path, sandbox },
            sequence: 0,
        }
    }

    /// The source that installs `tools` for this execution.
    pub(super) fn prelude(&self) -> String {
        format!(
            r#"globalThis.tools = (() => {{
  const path = {path}; const marker = {marker}; let seq = 0; let queue = Promise.resolve();
  const request = (payload) => {{
    const run = async () => {{
      const id = seq + 1;
      const frame = JSON.stringify({{id, ...payload}});
      if (Buffer.byteLength(frame) > {MAX_FRAME}) throw new Error("tools request exceeds 64 KiB");
      seq = id;
      process.stderr.write(marker + frame + "\n ");
      for (;;) {{
        let response;
        try {{ response = await Bun.file(path).json(); }} catch {{}}
        if (response && response.id === id) {{
          if (response.error !== undefined) throw new Error(response.error);
          return response.result;
        }}
        await Bun.sleep(5);
      }}
    }};
    const result = queue.then(run); queue = result.catch(() => {{}}); return result;
  }};
  return Object.freeze({{
    batch: (calls) => request({{calls}}),
    call: async (name, args) => {{
      const [result] = await request({{calls: [{{name, arguments: args ?? {{}}}}]}});
      if (result.is_error) throw new Error(result.output?.error ?? JSON.stringify(result.output));
      return result.output;
    }},
    list: () => request({{list: true}}),
  }});
}})();
"#,
            path = json!(self.file.path.to_string_lossy()),
            marker = json!(self.marker),
        )
    }

    /// Take the next complete request frame off `stderr`.
    pub(super) fn take_frame(&self, stderr: &mut Vec<u8>) -> Option<Vec<u8>> {
        let start = find(stderr, self.marker.as_bytes())?;
        let end = start + stderr[start..].iter().position(|b| *b == b'\n')?;
        let frame = stderr[start + self.marker.len()..end].to_vec();
        stderr.drain(start..=end);
        Some(frame)
    }

    /// Answer one frame. A tool or dispatch failure goes back to Bun with
    /// its own message; `Err` is a protocol failure the execution cannot
    /// recover from.
    pub(super) async fn answer(&mut self, frame: &[u8], ctx: &ToolContext) -> Result<(), String> {
        let request: Value = serde_json::from_slice(frame)
            .ok()
            .filter(|_| frame.len() <= MAX_FRAME)
            .ok_or("malformed tools request")?;
        let id = self.sequence + 1;
        if request["id"].as_u64() != Some(id) {
            return Err("out-of-sequence tools request".into());
        }
        self.sequence = id;
        let response = match self.respond(&request, ctx).await {
            Ok(result) => json!({"id": id, "result": result}),
            Err(error) => json!({"id": id, "error": error}),
        };
        let mut bytes = serde_json::to_vec(&response).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_RESPONSE {
            bytes = serde_json::to_vec(&json!({"id": id, "error": "tool results exceed 1 MiB"}))
                .map_err(|error| error.to_string())?;
        }
        self.write(&bytes).await
    }

    pub(super) async fn respond(
        &self,
        request: &Value,
        ctx: &ToolContext,
    ) -> Result<Value, String> {
        if request["list"] == true {
            return Ok(json!(self.dispatch.schemas()));
        }
        let calls: Vec<Call> = serde_json::from_value(request["calls"].clone())
            .map_err(|error| format!("invalid tools request: {error}"))?;
        if calls.is_empty() || calls.len() > MAX_BATCH {
            return Err(format!(
                "a tools batch holds 1 to {MAX_BATCH} calls, not {}",
                calls.len()
            ));
        }
        let calls = calls.into_iter().map(|c| (c.name, c.arguments)).collect();
        let results = self
            .dispatch
            .execute(ctx, calls)
            .await
            .map_err(|error| error.to_string())?;
        Ok(json!(results))
    }

    async fn write(&self, bytes: &[u8]) -> Result<(), String> {
        let path = &self.file.path;
        let Some(sandbox) = &self.file.sandbox else {
            // Written aside and renamed, so Bun never reads a partial file.
            let staged = path.with_extension("json.part");
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            return options
                .open(&staged)
                .and_then(|mut file| file.write_all(bytes))
                .and_then(|_| std::fs::rename(&staged, path))
                .map_err(|error| format!("failed to write the tools response: {error}"));
        };
        sandbox
            .write_file(
                &path.to_string_lossy(),
                bytes,
                orca_harness_core::FileMode::Regular,
            )
            .await
            .map_err(|error| format!("failed to write the tools response: {error}"))
    }
}

/// Marks the interpreter dirty when a programmatic execution is dropped
/// before it completes: its code may still be running, waiting on a
/// response nobody will write. The dispatcher can drop the tool future
/// before the tool's own cancellation branch runs, so the mark is set
/// synchronously, the next call checks it under the session lock before
/// reuse, and the kill is scheduled at once.
pub(super) struct DirtyOnDrop {
    session: Arc<tokio::sync::Mutex<Session>>,
    interrupted: Arc<AtomicBool>,
    pub(super) armed: bool,
}

impl DirtyOnDrop {
    pub(super) fn new(
        session: Arc<tokio::sync::Mutex<Session>>,
        interrupted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            session,
            interrupted,
            armed: true,
        }
    }
}

impl Drop for DirtyOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.interrupted.store(true, Ordering::SeqCst);
        let session = self.session.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let mut session = session.lock().await;
                // The next call may already have replaced the interpreter.
                if session.interrupted.load(Ordering::SeqCst) {
                    if let Some(live) = session.live.as_mut() {
                        live.process.kill().await;
                    }
                }
            });
        }
    }
}
