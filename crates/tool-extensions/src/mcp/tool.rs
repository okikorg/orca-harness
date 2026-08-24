//! One remote MCP tool exposed through the harness [`Tool`] trait.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use crate::mcp::client::{call_output, McpClient};

/// A tool listed by a connected server. Calls forward to `tools/call`;
/// the schema (already `mcp__<server>__` prefixed) came from
/// `tools/list` at connect time.
pub struct McpTool {
    client: Arc<McpClient>,
    schema: ToolSchema,
    /// The server-side name, without the host-facing prefix.
    remote: String,
}

impl McpTool {
    pub(crate) fn new(client: Arc<McpClient>, schema: ToolSchema, remote: String) -> Self {
        Self {
            client,
            schema,
            remote,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn schema(&self) -> ToolSchema {
        self.schema.clone()
    }

    /// The connection is one request/response lane, so calls into the
    /// same server serialize; different servers stay concurrent.
    fn concurrency(&self, _input: &Value) -> Concurrency {
        Concurrency::Keyed(format!("mcp:{}", self.client.server()))
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let exchange = self.client.request(
            "tools/call",
            json!({ "name": self.remote, "arguments": input }),
        );
        let expired = async {
            match ctx.deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            result = exchange => call_output(&result.map_err(|e| ToolError::msg(e.to_string()))?),
            _ = ctx.cancellation.cancelled() => Err(ToolError::msg("cancelled")),
            _ = expired => Err(ToolError::msg("deadline exceeded")),
        }
    }
}
