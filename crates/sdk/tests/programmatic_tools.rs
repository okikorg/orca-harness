//! `AgentBuilder::programmatic_tools` wiring that needs no interpreter:
//! the run offers the dispatching `bun_repl` in place of the plain one,
//! and every top-level call dispatches exactly as without the feature.
//! The Bun side is covered by `programmatic_bun.rs`.

use async_trait::async_trait;
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_sdk::{
    Context, FnTool, Harness, Message, Model, ModelError, ModelResponse, ProgrammaticTools,
    ToolSchema,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
mod common;

/// Records the schemas each model call was offered.
#[derive(Default)]
struct Offered(Mutex<Vec<ToolSchema>>);

#[async_trait]
impl Model for Offered {
    async fn generate(
        &self,
        _context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        *self.0.lock().unwrap() = tools.to_vec();
        Ok(ModelResponse::final_text("done"))
    }
}

async fn offered(programmatic: bool) -> Vec<ToolSchema> {
    let root = common::temp_dir("programmatic-offer");
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let model = Arc::new(Offered::default());
    let mut builder = harness
        .agent(model.clone())
        .tool(FnTool::new("before", "", json!({}), |_, _| async {
            Ok(json!(null))
        }))
        .bun()
        .tool(FnTool::new("after", "", json!({}), |_, _| async {
            Ok(json!(null))
        }));
    if programmatic {
        builder = builder.programmatic_tools(ProgrammaticTools::new());
    }
    let session = builder.build().unwrap().new_session().open().unwrap();
    session.run("go").await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
    let schemas = model.0.lock().unwrap().clone();
    schemas
}

#[tokio::test]
async fn the_dispatching_bun_repl_takes_the_plain_ones_place() {
    let plain = offered(false).await;
    let programmatic = offered(true).await;
    let names = |schemas: &[ToolSchema]| -> Vec<String> {
        schemas.iter().map(|schema| schema.name.clone()).collect()
    };
    assert_eq!(names(&programmatic), names(&plain));
    assert_eq!(names(&plain)[..3], ["before", "bun_repl", "after"]);
    let bun = |schemas: &[ToolSchema]| -> String {
        schemas
            .iter()
            .find(|schema| schema.name == "bun_repl")
            .unwrap()
            .description
            .clone()
    };
    assert!(!bun(&plain).contains("tools.call"));
    assert!(bun(&programmatic).starts_with(&bun(&plain)));
    assert!(bun(&programmatic).contains("tools.call"));
}

#[tokio::test]
async fn top_level_calls_dispatch_as_without_the_feature() {
    let root = common::temp_dir("programmatic-top-level");
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let model = Arc::new(ScriptedModel::tool_round(
        vec![
            call("unknown", "nope", json!({})),
            call("known", "echo", json!({"n": 1})),
        ],
        "done",
    ));
    let agent = harness
        .agent(model)
        .bun()
        .programmatic_tools(ProgrammaticTools::new())
        .tool(FnTool::new("echo", "", json!({}), |input, _| async move {
            Ok(input)
        }))
        .build()
        .unwrap();
    let session = agent.new_session().open().unwrap();
    session.run("go").await.unwrap();
    let results = session
        .messages()
        .await
        .into_iter()
        .find_map(|message| match message {
            Message::Tool { results } => Some(results),
            _ => None,
        })
        .unwrap();
    assert_eq!(results[0].call_id, "unknown");
    assert!(results[0].is_error);
    assert_eq!(results[0].output, json!({"error": "unknown tool: nope"}));
    assert_eq!(results[1].call_id, "known");
    assert_eq!(results[1].output, json!({"n": 1}));
    std::fs::remove_dir_all(root).unwrap();
}
