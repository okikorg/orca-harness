//! Record a scripted run, resume it, and continue: the loaded context
//! must equal what the extension saw, and appends must keep working.

use std::sync::Arc;

use async_trait::async_trait;
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    Agent, CancellationToken, Context, ModelResponse, Tool, ToolContext, ToolError, ToolSchema,
};
use orca_harness_extensions::{SessionFile, SessionHandler};

struct Echo;

#[async_trait]
impl Tool for Echo {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "echo".into(),
            description: "echo the input".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<serde_json::Value, ToolError> {
        Ok(input)
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("orca-session-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test]
async fn records_a_tool_round_and_resumes_it() {
    let dir = temp_dir("roundtrip");

    // Run 1: a tool round, recorded.
    let model = ScriptedModel::tool_round(
        vec![call("c1", "echo", serde_json::json!({"x": 1}))],
        "done",
    );
    let handler = Arc::new(SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap());
    let agent = Agent::new(model).extension_arc(handler.clone()).tool(Echo);
    let mut context = Context::new();
    context.push_system("sys");
    context.push_user("go");
    agent
        .run_context(&mut context, CancellationToken::new())
        .await
        .unwrap();

    let sessions = SessionFile::list(&dir);
    assert_eq!(sessions.len(), 1);
    let loaded = SessionFile::load(&sessions[0].path).unwrap();
    assert!(loaded.warnings.is_empty(), "warnings: {:?}", loaded.warnings);
    assert_eq!(
        serde_json::to_string(loaded.context.messages()).unwrap(),
        serde_json::to_string(context.messages()).unwrap(),
    );

    // Run 2: resume the same file and continue the conversation.
    let (handler, loaded) = SessionHandler::resume(&sessions[0].path).unwrap();
    let handler = Arc::new(handler);
    let model = ScriptedModel::new(vec![ModelResponse::final_text("again")]);
    let agent = Agent::new(model).extension_arc(handler.clone());
    let mut context = loaded.context;
    context.push_user("more");
    agent
        .run_context(&mut context, CancellationToken::new())
        .await
        .unwrap();

    let reloaded = SessionFile::load(&sessions[0].path).unwrap();
    assert_eq!(
        serde_json::to_string(reloaded.context.messages()).unwrap(),
        serde_json::to_string(context.messages()).unwrap(),
    );
    let _ = std::fs::remove_dir_all(&dir);
}
