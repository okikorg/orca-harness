//! Subagent tool driven by the scripted model — no network, fully
//! deterministic.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    CancellationToken, Context, Limits, Model, ModelError, ModelResponse, Tool, ToolContext,
    ToolError, ToolSchema, Usage,
};
use orca_harness_tools::{
    SubagentModel, SubagentTool, Workspace, AUTO_SUBAGENT_ROUTE, DEFAULT_SUBAGENT_MAX_STEPS,
    DEFAULT_SUBAGENT_TIMEOUT, MAX_SUBAGENT_MAX_STEPS, PREFERENCE_SUBAGENT_ROUTE,
};

static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn temp_ws() -> (Workspace, std::path::PathBuf) {
    let n = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("orca-harness-sub-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    (Workspace::new(dir.clone()), dir)
}

fn ctx() -> ToolContext {
    ToolContext {
        call_id: "t".into(),
        tool_name: "subagent".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

include!("subagent/execution.rs");
include!("subagent/routing.rs");
include!("subagent/events.rs");
