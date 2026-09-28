//! Syntactic mutation validation positioned by the host before approval.

use async_trait::async_trait;
use serde_json::Value;

use orca_harness_core::{Extension, Next, Subscriptions, ToolCall, ToolContext, ToolError};

use super::edit_spec;

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
        if call.name == "edit_file" {
            edit_spec::validate(&input)?;
        }
        next.run(input).await
    }
}
