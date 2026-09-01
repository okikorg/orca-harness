//! Syntactic mutation validation positioned by the host before approval.

use async_trait::async_trait;
use serde_json::Value;

use orca_harness_core::{Extension, Next, Subscriptions, ToolCall, ToolContext, ToolError};

use super::{multi_edit_spec, patch_format::parse_patch};

/// Rejects malformed native mutation calls before later `around_tool`
/// extensions spend time reviewing or sandboxing them.
///
/// Execution still performs the same validation again before any I/O. This
/// extension is only the cheap, host-positioned fast path for invalid syntax.
#[derive(Debug, Default, Clone, Copy)]
pub struct MutationPreflight;

#[async_trait]
impl Extension for MutationPreflight {
    fn name(&self) -> &str {
        "mutation-preflight"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().around_tool()
    }

    async fn around_tool<'a>(
        &self,
        call: &ToolCall,
        input: Value,
        _ctx: &ToolContext,
        next: Next<'a>,
    ) -> Result<Value, ToolError> {
        match call.name.as_str() {
            "multi_edit" => multi_edit_spec::validate(&input)?,
            "apply_patch" => {
                let patch = input
                    .get("patch")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::msg("`patch` (string) is required"))?;
                parse_patch(patch)?;
            }
            _ => {}
        }
        next.run(input).await
    }
}
