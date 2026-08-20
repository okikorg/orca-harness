//! `read_tool_result` — re-read a previously truncated tool output.
//!
//! The tool half of the [`Truncation`](crate::Truncation) +
//! [`TruncationStore`](crate::TruncationStore) pair: when a result is
//! trimmed, its full original stays in the store and the truncated output
//! carries a `_readFull` hint naming the `callId`. The model can then
//! page through the original in slices. Lives here rather than in the
//! tools crate because it is meaningless without the extension.
//!
//! Register both halves:
//!
//! ```no_run
//! use orca_harness_core::Agent;
//! use orca_harness_extensions::{ReadToolResultTool, Truncation, TruncationStore};
//!
//! # fn example(model: impl orca_harness_core::Model) {
//! let store = TruncationStore::default();
//! let agent = Agent::new(model)
//!     .extension(Truncation::default().store(store.clone()))
//!     .tool_arc(std::sync::Arc::new(ReadToolResultTool::new(store)));
//! # let _ = agent; }
//! ```

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::{Tool, ToolContext, ToolError, ToolSchema};

use crate::truncation::{TruncationStore, READ_TOOL_RESULT};

pub struct ReadToolResultTool {
    store: TruncationStore,
    /// Default slice size in characters.
    default_chars: usize,
    /// Hard cap on a requested slice. Slices bypass the truncation
    /// extension, so this is the only guard on context spend.
    max_chars: usize,
}

impl ReadToolResultTool {
    pub fn new(store: TruncationStore) -> Self {
        Self {
            store,
            default_chars: 8 * 1024,
            max_chars: 64 * 1024,
        }
    }

    pub fn max_chars(mut self, chars: usize) -> Self {
        self.max_chars = chars;
        self
    }
}

#[async_trait]
impl Tool for ReadToolResultTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: READ_TOOL_RESULT.into(),
            description: "Re-read the full output of an earlier tool call that was \
                truncated (marked `_truncated` with a `_readFull` hint). Returns a slice \
                of the original serialized output; page with `offset` until `hasMore` is \
                false."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "callId": {"type": "string", "description": "Id of the truncated tool call."},
                    "offset": {"type": "integer", "default": 0, "description": "Character offset to start from."},
                    "maxChars": {"type": "integer", "description": "Slice size in characters (default 8192)."}
                },
                "required": ["callId"]
            }),
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let call_id = input
            .get("callId")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`callId` (string) is required"))?;
        let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let max_chars = input
            .get("maxChars")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(self.default_chars)
            .clamp(1, self.max_chars);

        let (tool_name, full) = self.store.get(call_id).ok_or_else(|| {
            ToolError::msg(format!(
                "no stored output for callId {call_id}; only truncated results are \
                 retained, and old entries are evicted"
            ))
        })?;

        let total_chars = full.chars().count();
        let content: String = full.chars().skip(offset).take(max_chars).collect();
        let end = offset.saturating_add(content.chars().count());
        Ok(json!({
            "callId": call_id,
            "toolName": tool_name,
            "totalChars": total_chars,
            "offset": offset,
            "content": content,
            "hasMore": end < total_chars,
            "nextOffset": if end < total_chars { Some(end) } else { None },
        }))
    }
}
