//! The model-facing `process` tool: JSON parsing, schema, concurrency,
//! and dispatch onto the shared [`ProcessCore`] implementation.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use super::{ProcessEntry, ProcessSpawn, ProcessTool, ProcessWrite};

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
            "list" => return Ok(ProcessEntry::list_into_value(core.list()?)),
            other => {
                return Err(ToolError::msg(format!(
                    "unknown action `{other}`; expected spawn|poll|write|kill|list"
                )))
            }
        };
        Ok(snapshot.into_value())
    }
}
