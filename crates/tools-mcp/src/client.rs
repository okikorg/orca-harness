//! One stdio MCP server connection: spawn, handshake, request/response.
//!
//! The wire is a single lane — requests are serialized behind a mutex and
//! each response is matched by id, so an exchange abandoned mid-flight
//! (cancellation) leaves at worst a stale response the next exchange
//! skips over. Server-initiated traffic is tolerated, not supported:
//! notifications are ignored, `ping` is answered, anything else is
//! refused with a JSON-RPC error.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use orca_harness_core::{Tool, ToolSchema};

use crate::tool::McpTool;

/// The protocol revision offered in `initialize`. Servers may answer
/// with their own; nothing later in the exchange depends on which won.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Cap on each handshake step so a broken command cannot wedge the
/// caller. Generous because `npx`-style launchers download on first run.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("spawn failed: {0}")]
    Spawn(#[from] std::io::Error),

    #[error("{step} timed out")]
    Timeout { step: &'static str },

    #[error("{0}")]
    Protocol(String),
}

/// A connected MCP server. Held via `Arc` by every [`McpTool`] it
/// produced; the child process is killed when the last clone drops.
pub struct McpClient {
    server: String,
    next_id: AtomicU64,
    wire: Mutex<Wire>,
}

struct Wire {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    // Held for kill_on_drop; all traffic goes through the taken pipes.
    _child: Child,
}

impl McpClient {
    /// Spawn `command` (split on whitespace — no shell quoting), run the
    /// MCP handshake, and return one [`Tool`] per tool the server lists,
    /// each named `mcp__<server>__<tool>`.
    pub async fn connect(server: &str, command: &str) -> Result<Vec<Arc<dyn Tool>>, McpError> {
        let mut parts = command.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| McpError::Protocol("empty command".into()))?;
        let mut child = Command::new(program)
            .args(parts)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // A server's diagnostics must not corrupt the host terminal.
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
        let client = Arc::new(Self {
            server: server.to_string(),
            next_id: AtomicU64::new(1),
            wire: Mutex::new(Wire {
                stdin,
                stdout,
                _child: child,
            }),
        });

        let init = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "orca-harness",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        step(
            HANDSHAKE_TIMEOUT,
            "initialize",
            client.request("initialize", init),
        )
        .await?;
        client.notify("notifications/initialized").await?;
        let listed = step(
            HANDSHAKE_TIMEOUT,
            "tools/list",
            client.request("tools/list", json!({})),
        )
        .await?;

        Ok(parse_tool_list(server, &listed)?
            .into_iter()
            .map(|(schema, remote)| {
                Arc::new(McpTool::new(client.clone(), schema, remote)) as Arc<dyn Tool>
            })
            .collect())
    }

    /// The configured server name (also the tools' concurrency key).
    pub fn server(&self) -> &str {
        &self.server
    }

    /// One request/response exchange. Callers wanting cancellation or a
    /// deadline select over this future; see [`Wire::recv`] for why an
    /// abandoned exchange is harmless.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut wire = self.wire.lock().await;
        wire.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        wire.recv(id).await
    }

    async fn notify(&self, method: &str) -> Result<(), McpError> {
        let mut wire = self.wire.lock().await;
        wire.send(&json!({ "jsonrpc": "2.0", "method": method }))
            .await
    }
}

impl Wire {
    async fn send(&mut self, message: &Value) -> Result<(), McpError> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Read until the response for `id` arrives. Notifications and
    /// non-JSON lines (servers that log to stdout) are skipped; stale
    /// responses from abandoned exchanges have smaller ids and are
    /// skipped too; server requests are answered inline so a `ping`
    /// mid-exchange cannot deadlock the lane.
    async fn recv(&mut self, id: u64) -> Result<Value, McpError> {
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line).await? == 0 {
                return Err(McpError::Protocol("server closed the connection".into()));
            }
            let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if message["method"].is_string() {
                let request_id = &message["id"];
                if !request_id.is_null() {
                    let reply = if message["method"] == "ping" {
                        json!({ "jsonrpc": "2.0", "id": request_id, "result": {} })
                    } else {
                        json!({ "jsonrpc": "2.0", "id": request_id,
                                "error": { "code": -32601, "message": "method not supported" } })
                    };
                    self.send(&reply).await?;
                }
                continue;
            }
            if message["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let detail = error["message"].as_str().unwrap_or("unknown error");
                return Err(McpError::Protocol(format!("server error: {detail}")));
            }
            return Ok(message["result"].clone());
        }
    }
}

async fn step(
    limit: std::time::Duration,
    name: &'static str,
    exchange: impl std::future::Future<Output = Result<Value, McpError>>,
) -> Result<Value, McpError> {
    tokio::time::timeout(limit, exchange)
        .await
        .map_err(|_| McpError::Timeout { step: name })?
}

/// A `tools/list` result as (model-facing schema, server-side tool name)
/// pairs. Nameless entries are a protocol violation; entries without a
/// description or input schema are common and get harmless defaults.
fn parse_tool_list(server: &str, result: &Value) -> Result<Vec<(ToolSchema, String)>, McpError> {
    let tools = result["tools"]
        .as_array()
        .ok_or_else(|| McpError::Protocol("tools/list returned no tools array".into()))?;
    tools
        .iter()
        .map(|tool| {
            let remote = tool["name"]
                .as_str()
                .ok_or_else(|| McpError::Protocol("tools/list entry without a name".into()))?
                .to_string();
            let parameters = match &tool["inputSchema"] {
                Value::Object(schema) => Value::Object(schema.clone()),
                _ => json!({ "type": "object" }),
            };
            let schema = ToolSchema {
                name: format!("mcp__{server}__{remote}"),
                description: tool["description"].as_str().unwrap_or("").to_string(),
                parameters,
            };
            Ok((schema, remote))
        })
        .collect()
}

/// Reduce a `tools/call` result to the model-visible output value:
/// `structuredContent` verbatim when present, text content joined,
/// anything richer passed through raw.
pub(crate) fn call_output(result: &Value) -> Result<Value, orca_harness_core::ToolError> {
    let text = result["content"].as_array().map(|items| {
        items.iter().all(|item| item["type"] == "text").then(|| {
            items
                .iter()
                .filter_map(|item| item["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
    });
    if result["isError"].as_bool() == Some(true) {
        let detail = match text.flatten().filter(|t| !t.is_empty()) {
            Some(text) => text,
            None => "tool reported an error".to_string(),
        };
        return Err(orca_harness_core::ToolError::msg(detail));
    }
    if let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null()) {
        return Ok(structured.clone());
    }
    Ok(match text {
        Some(Some(text)) => json!({ "content": text }),
        _ => json!({ "content": result["content"].clone() }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_prefixes_names_and_defaults_missing_fields() {
        let listed = json!({ "tools": [
            { "name": "echo", "description": "echo back",
              "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } } },
            { "name": "bare" },
        ]});
        let tools = parse_tool_list("docs", &listed).unwrap();
        assert_eq!(tools[0].0.name, "mcp__docs__echo");
        assert_eq!(tools[0].0.description, "echo back");
        assert_eq!(tools[0].1, "echo");
        assert_eq!(tools[1].0.name, "mcp__docs__bare");
        assert_eq!(tools[1].0.description, "");
        assert_eq!(tools[1].0.parameters, json!({ "type": "object" }));
    }

    #[test]
    fn tool_list_rejects_malformed_results() {
        assert!(parse_tool_list("d", &json!({})).is_err());
        assert!(
            parse_tool_list("d", &json!({ "tools": [{ "description": "nameless" }] })).is_err()
        );
    }

    #[test]
    fn call_output_joins_text_and_prefers_structured_content() {
        let text = json!({ "content": [
            { "type": "text", "text": "one" },
            { "type": "text", "text": "two" },
        ]});
        assert_eq!(
            call_output(&text).unwrap(),
            json!({ "content": "one\ntwo" })
        );

        let structured = json!({ "content": [], "structuredContent": { "count": 3 } });
        assert_eq!(call_output(&structured).unwrap(), json!({ "count": 3 }));

        // Non-text content passes through raw rather than being dropped.
        let image = json!({ "content": [{ "type": "image", "data": "abc" }] });
        assert_eq!(
            call_output(&image).unwrap(),
            json!({ "content": [{ "type": "image", "data": "abc" }] })
        );
    }

    #[test]
    fn call_output_surfaces_is_error_as_a_tool_error() {
        let failed = json!({ "isError": true, "content": [{ "type": "text", "text": "boom" }] });
        assert_eq!(call_output(&failed).unwrap_err().message, "boom");
        let silent = json!({ "isError": true, "content": [] });
        assert_eq!(
            call_output(&silent).unwrap_err().message,
            "tool reported an error"
        );
    }
}
