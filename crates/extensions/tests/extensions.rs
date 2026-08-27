//! The critical extensions, exercised end-to-end through the Agent with a
//! scripted fake LLM.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    Agent, CancellationToken, FnTool, Limits, Model, ModelError, ModelResponse, ToolError, Usage,
};
use orca_harness_extensions::{
    EventStream, HarnessEvent, PolicyOutcome, RetryModel, ToolPolicy, ToolRetry, Truncation,
    UsageMeter,
};

const RUN_TIMEOUT: Duration = Duration::from_secs(10);

fn echo() -> FnTool {
    FnTool::new(
        "echo",
        "echoes input",
        json!({"type": "object"}),
        |input, _ctx| async move { Ok(input) },
    )
}

include!("extensions/events.rs");
include!("extensions/tool_policy_retry.rs");
include!("extensions/model_retry_usage.rs");
