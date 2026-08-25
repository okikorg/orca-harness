//! One remote MCP tool exposed through the harness [`Tool`] trait.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use crate::mcp::client::{call_output, request_with_context, McpClient};

/// A tool listed by a connected server. Calls forward to `tools/call`;
/// the schema (already `mcp__<server>__` prefixed) came from
/// `tools/list` at connect time.
pub struct McpTool {
    client: Arc<McpClient>,
    schema: ToolSchema,
    /// The server-side name, without the host-facing prefix.
    remote: String,
    selected: AtomicBool,
}

impl McpTool {
    pub(crate) fn new(client: Arc<McpClient>, schema: ToolSchema, remote: String) -> Self {
        Self {
            client,
            schema,
            remote,
            selected: AtomicBool::new(false),
        }
    }

    pub(crate) fn remote_name(&self) -> &str {
        &self.remote
    }

    pub(crate) fn is_selected(&self) -> bool {
        self.selected.load(Ordering::Acquire)
    }

    /// Select idempotently; returns whether it had already been selected.
    pub(crate) fn select(&self) -> bool {
        self.selected.swap(true, Ordering::AcqRel)
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
        if !input.is_object() {
            return Err(ToolError::msg("MCP tool arguments must be an object"));
        }
        if !self.is_selected() {
            return Err(ToolError::msg(format!(
                "MCP tool {} is not selected; call mcp_select_tool first",
                self.schema.name
            )));
        }
        let result = request_with_context(
            &self.client,
            "tools/call",
            json!({ "name": self.remote, "arguments": input }),
            ctx,
        )
        .await?;
        call_output(&result)
    }
}
