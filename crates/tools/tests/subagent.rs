//! Subagent tool driven by the scripted model — no network, fully
//! deterministic.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    CancellationToken, Context, Limits, Model, ModelError, ModelResponse, Tool, ToolContext,
    ToolSchema, Usage,
};
use orca_harness_tools::{SubagentTool, Workspace};

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

#[tokio::test]
async fn runs_task_and_reports_answer_and_usage() {
    let model = Arc::new(ScriptedModel::new(vec![ModelResponse::Final {
        text: "found 3 files".into(),
        usage: Some(Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        }),
    }]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model, &ws);
    let out = tool.call(json!({"task": "count files"}), &ctx()).await.unwrap();
    assert_eq!(out["answer"], "found 3 files");
    assert_eq!(out["usage"]["inputTokens"], 10);
    assert_eq!(out["usage"]["outputTokens"], 5);
}

#[tokio::test]
async fn inner_agent_executes_real_tools() {
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call(
            "1",
            "write_file",
            json!({"path": "note.txt", "content": "from the subagent"}),
        )],
        "wrote it",
    ));
    let (ws, dir) = temp_ws();
    let tool = SubagentTool::new(model, &ws);
    let out = tool.call(json!({"task": "write a note"}), &ctx()).await.unwrap();
    assert_eq!(out["answer"], "wrote it");
    assert_eq!(
        std::fs::read_to_string(dir.join("note.txt")).unwrap(),
        "from the subagent"
    );
}

/// A model that never answers — for cancellation tests.
struct StallModel;

#[async_trait]
impl Model for StallModel {
    async fn generate(
        &self,
        _context: &Context,
        _tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        tokio::time::sleep(Duration::from_secs(300)).await;
        Err(ModelError::InvalidResponse("unreachable".into()))
    }
}

#[tokio::test]
async fn parent_cancellation_reaches_the_inner_run() {
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(Arc::new(StallModel), &ws);
    let cancel = CancellationToken::new();
    let tctx = ToolContext {
        call_id: "t".into(),
        tool_name: "subagent".into(),
        cancellation: cancel.clone(),
        deadline: None,
    };
    let handle = tokio::spawn(async move { tool.call(json!({"task": "stall"}), &tctx).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("cancellation must end the inner run promptly")
        .unwrap();
    assert!(result.is_err());
}

#[tokio::test]
async fn step_limit_exhaustion_is_a_tool_error() {
    // One permitted step that returns tool calls: the run cannot finish.
    let model = Arc::new(ScriptedModel::new(vec![ModelResponse::tool_calls(vec![
        call("1", "list_dir", json!({"path": "."})),
    ])]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model, &ws).limits(Limits {
        max_steps: 1,
        ..Limits::default()
    });
    let result = tool.call(json!({"task": "loop forever"}), &ctx()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn task_is_required() {
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(Arc::new(StallModel), &ws);
    let err = tool.call(json!({}), &ctx()).await.unwrap_err();
    assert!(err.to_string().contains("task"));
}
