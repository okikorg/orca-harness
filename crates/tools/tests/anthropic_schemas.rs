//! The Anthropic Messages API rejects `oneOf`/`anyOf`/`allOf` at the top level
//! of a tool's `input_schema`. `workflow` and `subagent` both declare one, so
//! the adapter must strip it before the request goes out.
use orca_harness_core::{Context, Model, ModelError, ModelResponse, Tool, ToolSchema};
use orca_harness_model_providers::anthropic::input_schema;
use orca_harness_tools::{SubagentManager, SubagentTool, WorkflowStore, WorkflowTool};
use std::sync::Arc;

#[derive(Clone)]
struct Idle;

#[async_trait::async_trait]
impl Model for Idle {
    async fn generate(&self, _: &Context, _: &[ToolSchema]) -> Result<ModelResponse, ModelError> {
        Ok(ModelResponse::final_text(String::new()))
    }
}

fn combinator_tools() -> Vec<ToolSchema> {
    let subagent = Arc::new(
        SubagentTool::with_tools(Idle, Arc::new(Vec::new))
            .background(SubagentManager::new(1), |_| {}),
    );
    let workflow = WorkflowTool::new(subagent.clone(), WorkflowStore::new()).unwrap();
    vec![subagent.schema(), workflow.schema()]
}

#[test]
fn top_level_combinators_are_stripped_before_the_request() {
    for schema in combinator_tools() {
        // The shared schema keeps its combinator for every other provider.
        assert!(
            schema.parameters.get("oneOf").is_some(),
            "`{}` no longer declares a top-level `oneOf`; update this test",
            schema.name
        );
        let sent = input_schema(&schema.parameters);
        for keyword in ["oneOf", "anyOf", "allOf"] {
            assert!(
                sent.get(keyword).is_none(),
                "`{}` sends a top-level `{keyword}` to Anthropic",
                schema.name
            );
        }
        assert_eq!(sent["properties"], schema.parameters["properties"]);
        assert_eq!(sent["required"], schema.parameters["required"]);
    }
}
