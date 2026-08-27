//! One stdio MCP server connection: spawn, handshake, request/response.
//!
//! The wire is a single lane: requests are serialized behind a mutex and
//! each response is matched by id. Cancellation or deadline expiry closes
//! the connection rather than risking a partial write or stale exchange.
//! Server-initiated traffic is tolerated, not supported: notifications are
//! ignored, `ping` is answered, anything else is refused with a JSON-RPC error.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use orca_harness_core::{ToolContext, ToolError, ToolSchema};

use crate::mcp::tool::McpTool;

/// Protocol revisions implemented by this client, newest first.
const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[PROTOCOL_VERSION, "2025-03-26"];

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

/// One live connection and the tools discovered during its handshake.
pub struct McpConnection {
    client: Arc<McpClient>,
    tools: Vec<Arc<McpTool>>,
}

impl McpConnection {
    pub fn tools(&self) -> &[Arc<McpTool>] {
        &self.tools
    }

    pub fn into_parts(self) -> (Arc<McpClient>, Vec<Arc<McpTool>>) {
        (self.client, self.tools)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ServerCapabilities {
    pub(crate) tools: bool,
    pub(crate) resources: bool,
    pub(crate) prompts: bool,
    pub(crate) completions: bool,
}

/// A connected MCP server. Held via `Arc` by every [`McpTool`] it
/// produced; the child process is killed when the last clone drops.
pub struct McpClient {
    server: String,
    next_id: AtomicU64,
    healthy: AtomicBool,
    capabilities: OnceLock<ServerCapabilities>,
    child: StdMutex<Child>,
    wire: Mutex<Wire>,
}

struct Wire {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl McpClient {
    /// Spawn `command` (split on whitespace — no shell quoting), run the
    /// MCP handshake, and retain the live client plus the server's tools.
    pub async fn connect(server: &str, command: &str) -> Result<McpConnection, McpError> {
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
            healthy: AtomicBool::new(true),
            capabilities: OnceLock::new(),
            child: StdMutex::new(child),
            wire: Mutex::new(Wire { stdin, stdout }),
        });

        let init = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "orca-harness",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        let initialized = step(
            HANDSHAKE_TIMEOUT,
            "initialize",
            client.request("initialize", init),
        )
        .await?;
        let capabilities = parse_initialize(&initialized)?;
        client
            .capabilities
            .set(capabilities.clone())
            .expect("capabilities set once");
        client.notify("notifications/initialized").await?;

        let listed = if capabilities.tools {
            step(HANDSHAKE_TIMEOUT, "tools/list", list_all_tools(&client)).await?
        } else {
            Vec::new()
        };
        let tools = listed
            .into_iter()
            .map(|(schema, remote)| Arc::new(McpTool::new(client.clone(), schema, remote)))
            .collect();
        Ok(McpConnection { client, tools })
    }

    /// The configured server name (also the tools' concurrency key).
    pub fn server(&self) -> &str {
        &self.server
    }

    pub(crate) fn capabilities(&self) -> &ServerCapabilities {
        self.capabilities.get().expect("MCP handshake completed")
    }

    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    fn ensure_healthy(&self) -> Result<(), McpError> {
        self.healthy
            .load(Ordering::Acquire)
            .then_some(())
            .ok_or_else(|| {
                McpError::Protocol(
                    "connection closed after an interrupted request; reconnect the server".into(),
                )
            })
    }

    fn invalidate(&self) {
        if !self.healthy.swap(false, Ordering::AcqRel) {
            return;
        }
        let _ = self.child.lock().expect("MCP child lock").start_kill();
    }

    /// One request/response exchange.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        self.ensure_healthy()?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut wire = self.wire.lock().await;
        self.ensure_healthy()?;
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

/// Run a request with the same cancellation/deadline semantics as tools/call.
pub(crate) async fn request_with_context(
    client: &McpClient,
    method: &str,
    params: Value,
    ctx: &ToolContext,
) -> Result<Value, ToolError> {
    let exchange = client.request(method, params);
    let expired = async {
        match ctx.deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        result = exchange => result.map_err(|error| ToolError::msg(error.to_string())),
        _ = ctx.cancellation.cancelled() => {
            client.invalidate();
            Err(ToolError::msg("cancelled"))
        },
        _ = expired => {
            client.invalidate();
            Err(ToolError::msg("deadline exceeded"))
        },
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

    /// Read until the response for `id` arrives. Valid notifications are
    /// skipped, and server requests are answered inline so a `ping`
    /// mid-exchange cannot deadlock the lane. Non-JSON stdout diagnostics
    /// are tolerated; parsed JSON must be a valid JSON-RPC envelope.
    async fn recv(&mut self, id: u64) -> Result<Value, McpError> {
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line).await? == 0 {
                return Err(McpError::Protocol("server closed the connection".into()));
            }
            let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let object = message
                .as_object()
                .ok_or_else(|| McpError::Protocol("JSON-RPC message is not an object".into()))?;
            if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                return Err(McpError::Protocol("invalid JSON-RPC version".into()));
            }
            if let Some(method) = object.get("method") {
                let method = method
                    .as_str()
                    .ok_or_else(|| McpError::Protocol("JSON-RPC method is not a string".into()))?;
                if let Some(request_id) = object.get("id") {
                    let reply = if method == "ping" {
                        json!({ "jsonrpc": "2.0", "id": request_id, "result": {} })
                    } else {
                        json!({ "jsonrpc": "2.0", "id": request_id,
                                "error": { "code": -32601, "message": "method not supported" } })
                    };
                    self.send(&reply).await?;
                }
                continue;
            }
            let response_id = object
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| McpError::Protocol("JSON-RPC response has an invalid id".into()))?;
            if response_id != id {
                return Err(McpError::Protocol(format!(
                    "JSON-RPC response id {response_id} does not match request {id}"
                )));
            }
            let result = object.get("result");
            let error = object.get("error");
            if result.is_some() == error.is_some() {
                return Err(McpError::Protocol(
                    "JSON-RPC response must contain exactly one of result or error".into(),
                ));
            }
            if let Some(error) = error {
                let error = error
                    .as_object()
                    .ok_or_else(|| McpError::Protocol("JSON-RPC error is not an object".into()))?;
                if error.get("code").and_then(Value::as_i64).is_none() {
                    return Err(McpError::Protocol(
                        "JSON-RPC error code is not an integer".into(),
                    ));
                }
                let detail = error
                    .get("message")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        McpError::Protocol("JSON-RPC error message is not a string".into())
                    })?;
                return Err(McpError::Protocol(format!("server error: {detail}")));
            }
            return Ok(result.expect("validated result").clone());
        }
    }
}

async fn step<T>(
    limit: std::time::Duration,
    name: &'static str,
    exchange: impl std::future::Future<Output = Result<T, McpError>>,
) -> Result<T, McpError> {
    tokio::time::timeout(limit, exchange)
        .await
        .map_err(|_| McpError::Timeout { step: name })?
}

fn parse_initialize(result: &Value) -> Result<ServerCapabilities, McpError> {
    let object = result
        .as_object()
        .ok_or_else(|| McpError::Protocol("initialize result is not an object".into()))?;
    let version = object
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::Protocol("initialize returned no protocolVersion".into()))?;
    if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
        return Err(McpError::Protocol(format!(
            "unsupported MCP protocol version: {version}"
        )));
    }
    let capabilities = object
        .get("capabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| McpError::Protocol("initialize returned no capabilities object".into()))?;
    let server_info = object
        .get("serverInfo")
        .and_then(Value::as_object)
        .ok_or_else(|| McpError::Protocol("initialize returned no serverInfo object".into()))?;
    for field in ["name", "version"] {
        if server_info.get(field).and_then(Value::as_str).is_none() {
            return Err(McpError::Protocol(format!(
                "initialize serverInfo has no {field}"
            )));
        }
    }
    for capability in ["tools", "resources", "prompts", "completions"] {
        if capabilities
            .get(capability)
            .is_some_and(|value| !value.is_object())
        {
            return Err(McpError::Protocol(format!(
                "initialize capability {capability} is not an object"
            )));
        }
    }
    Ok(ServerCapabilities {
        tools: capabilities.contains_key("tools"),
        resources: capabilities.contains_key("resources"),
        prompts: capabilities.contains_key("prompts"),
        completions: capabilities.contains_key("completions"),
    })
}

struct ToolPage {
    tools: Vec<(ToolSchema, String)>,
    next_cursor: Option<String>,
}

fn ensure_unique_tools(tools: &[(ToolSchema, String)]) -> Result<(), McpError> {
    let mut names = HashSet::new();
    if let Some(duplicate) = tools
        .iter()
        .map(|(schema, _)| schema.name.as_str())
        .find(|name| !names.insert(*name))
    {
        return Err(McpError::Protocol(format!(
            "duplicate MCP tool name: {duplicate}"
        )));
    }
    Ok(())
}

async fn list_all_tools(client: &McpClient) -> Result<Vec<(ToolSchema, String)>, McpError> {
    let mut cursor = None;
    let mut seen = HashSet::new();
    let mut tools = Vec::new();
    loop {
        let params = cursor
            .as_ref()
            .map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
        let result = client.request("tools/list", params).await?;
        let page = parse_tool_page(client.server(), &result)?;
        tools.extend(page.tools);
        let Some(next) = page.next_cursor else {
            break;
        };
        if !seen.insert(next.clone()) {
            return Err(McpError::Protocol(format!(
                "tools/list repeated cursor: {next}"
            )));
        }
        cursor = Some(next);
    }
    ensure_unique_tools(&tools)?;
    Ok(tools)
}

/// Parse one `tools/list` page. Nameless entries are a protocol violation;
/// entries without a description or input schema get harmless defaults.
fn parse_tool_page(server: &str, result: &Value) -> Result<ToolPage, McpError> {
    let object = result
        .as_object()
        .ok_or_else(|| McpError::Protocol("tools/list result is not an object".into()))?;
    let tools = object
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| McpError::Protocol("tools/list returned no tools array".into()))?;
    let tools = tools
        .iter()
        .map(|tool| {
            let remote = tool["name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| McpError::Protocol("tools/list entry without a name".into()))?
                .to_string();
            let parameters = tool
                .get("inputSchema")
                .and_then(Value::as_object)
                .map(|schema| Value::Object(schema.clone()))
                .ok_or_else(|| {
                    McpError::Protocol("tools/list entry without an inputSchema object".into())
                })?;
            let schema = ToolSchema {
                name: format!("mcp__{server}__{remote}"),
                description: tool["description"].as_str().unwrap_or("").to_string(),
                parameters,
            };
            Ok((schema, remote))
        })
        .collect::<Result<Vec<_>, McpError>>()?;
    let next_cursor = match object.get("nextCursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(cursor)) if !cursor.is_empty() => Some(cursor.clone()),
        Some(_) => {
            return Err(McpError::Protocol(
                "tools/list nextCursor is not a non-empty string".into(),
            ))
        }
    };
    Ok(ToolPage { tools, next_cursor })
}

/// Reduce a `tools/call` result to the model-visible output value:
/// `structuredContent` verbatim when present, text content joined,
/// anything richer passed through raw.
pub(crate) fn call_output(result: &Value) -> Result<Value, orca_harness_core::ToolError> {
    let object = result
        .as_object()
        .ok_or_else(|| orca_harness_core::ToolError::msg("tools/call result is not an object"))?;
    if object
        .get("isError")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(orca_harness_core::ToolError::msg(
            "tools/call isError is not a boolean",
        ));
    }
    let content = object
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| orca_harness_core::ToolError::msg("tools/call returned no content array"))?;
    let text = Some(content.iter().all(|item| item["type"] == "text").then(|| {
        content
            .iter()
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }));
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
#[path = "client/tests.rs"]
mod tests;
