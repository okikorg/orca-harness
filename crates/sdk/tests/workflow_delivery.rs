//! What a workflow run delivers and to whom: every stage completion is
//! observable on the session's notification channel, but the parent
//! transcript receives exactly one completion per run, carrying the
//! run-level outcome; a cancelled run delivers a cancelled outcome once
//! its children have settled; and a stage runs under the same child
//! policy as any other worker, its denial visible in the outcome.

use std::sync::Arc;

use async_trait::async_trait;
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    CancellationToken, Context, FnTool, Message, Model, ModelError, ModelResponse, ToolSchema,
};
use orca_harness_sdk::orchestration::{RunState, StageStatus, SubagentModel};
use orca_harness_sdk::{
    BackgroundNotification, Harness, SubagentConfig, ToolPolicy, WorkflowSubmission,
};
use serde_json::{json, Value};

mod background_support;
mod common;
use background_support::{
    completion_messages, route_to_child, routed_agent, stage, wait_until, Echo, Held,
};
use common::temp_dir;

/// The one delivered batch's JSON, asserting there is exactly one.
fn delivered_batch(messages: &[Message]) -> Value {
    let delivered = completion_messages(messages);
    assert_eq!(
        delivered.len(),
        1,
        "one batch, one user turn: {delivered:?}"
    );
    serde_json::from_str(&delivered[0]).expect("a JSON batch")
}

#[tokio::test]
async fn stage_notifications_are_observed_but_only_the_run_outcome_is_delivered() {
    let root = temp_dir("workflow-delivery-stages");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let graph = json!([
        {"id": "a", "prompt": "alpha"},
        {"id": "b", "needs": ["a"], "prompt": "beta {{ stages.a.output }}"}
    ]);
    let parent = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call(
            "w1",
            "workflow",
            json!({"action": "run", "graph": graph}),
        )]),
        ModelResponse::final_text("submitted"),
        ModelResponse::final_text("reported"),
    ]));
    let agent = routed_agent(&harness, parent, Arc::new(Echo), SubagentConfig::new());
    let session = agent.new_session().ephemeral().open().unwrap();
    route_to_child(&session.subagents().unwrap().settings());
    let mut events = session.notifications();

    assert_eq!(session.run("review").await.unwrap().text, "submitted");
    wait_until(|| session.pending_completions() == 1, "the run outcome").await;
    assert!(
        completion_messages(&session.messages().await).is_empty(),
        "nothing is delivered between runs"
    );

    assert_eq!(session.run("and?").await.unwrap().text, "reported");
    let batch = delivered_batch(&session.messages().await);
    assert_eq!(batch["count"], 1, "{batch}");
    let completion = &batch["completions"][0];
    let task = completion["task"].as_str().unwrap();
    assert!(task.starts_with("workflow"), "{task}");
    assert_eq!(completion["parentId"], Value::Null, "a run, not a stage");
    let result = &completion["outcome"]["result"];
    assert_eq!(result["termination"], "completed", "{result}");
    let workflow = &result["workflow"];
    assert_eq!(workflow["state"], "done", "{workflow}");
    assert_eq!(workflow["outputs"], json!({"b": "beta alpha"}));
    assert_eq!(workflow["stages"], json!({"a": "done", "b": "done"}));
    assert_eq!(session.pending_completions(), 0);
    let run_id = completion["spawnId"].as_u64().unwrap();

    let mut stages = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let BackgroundNotification::SubagentFinished(n) = event {
            if n.spawn.run.is_some() {
                assert_eq!(n.spawn.run, Some(run_id));
                assert_eq!(n.spawn.call_id, "w1", "anchored to the parent's call");
                assert_eq!(n.result.unwrap()["answer"], n.spawn.task, "{}", n.spawn.id);
                stages.push(n.spawn.stage.unwrap());
            }
        }
    }
    stages.sort();
    assert_eq!(stages, ["a", "b"], "every stage completion was observable");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn cancelling_a_workflow_settles_its_children_and_delivers_a_cancelled_outcome() {
    let root = temp_dir("workflow-delivery-cancel");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let parent = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text("later")]));
    let agent = routed_agent(
        &harness,
        parent,
        Arc::new(Held(CancellationToken::new())),
        SubagentConfig::new(),
    );
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    let workflows = session.workflows().unwrap();
    route_to_child(&subagents.settings());

    let ack = workflows
        .submit(WorkflowSubmission::new([
            stage("a", "held", &[]),
            stage("b", "held too", &[]),
        ]))
        .unwrap();
    assert_eq!(subagents.active().len(), 3, "the run and two stages");

    workflows.cancel(ack.run_id).unwrap();
    wait_until(
        || subagents.active().is_empty(),
        "the stage workers to exit",
    )
    .await;
    wait_until(|| session.pending_completions() == 1, "the run outcome").await;
    let status = workflows.status(ack.run_id).unwrap();
    assert_eq!(status.state, RunState::Cancelled);
    assert_eq!(status.stages["a"], StageStatus::Cancelled);
    assert_eq!(status.stages["b"], StageStatus::Cancelled);
    assert!(workflows.runs().is_empty());

    assert_eq!(session.run("next").await.unwrap().text, "later");
    let batch = delivered_batch(&session.messages().await);
    let completion = &batch["completions"][0];
    assert_eq!(completion["spawnId"], ack.run_id);
    assert_eq!(completion["outcome"]["status"], "failed", "{completion}");
    let error = completion["outcome"]["error"].as_str().unwrap();
    assert!(error.starts_with("workflow cancelled"), "{error}");
    let outcome: Value = serde_json::from_str(error.split_once('\n').unwrap().1).unwrap();
    assert_eq!(outcome["state"], "cancelled");
    assert_eq!(
        outcome["stages"],
        json!({"a": "cancelled", "b": "cancelled"})
    );
    assert_eq!(session.pending_completions(), 0);

    let _ = std::fs::remove_dir_all(&root);
}

/// Calls the `forbidden` tool once, then answers with whatever the tool
/// result said, so a policy denial surfaces as the stage's output.
struct Reporter;

#[async_trait]
impl Model for Reporter {
    async fn generate(
        &self,
        context: &Context,
        _: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        Ok(match context.messages().last() {
            Some(Message::Tool { results }) => {
                ModelResponse::final_text(results[0].output.to_string())
            }
            _ => ModelResponse::tool_calls(vec![call("f1", "forbidden", json!({}))]),
        })
    }
}

#[tokio::test]
async fn workflow_stage_policy_denial_is_reported_in_the_outcome() {
    let root = temp_dir("workflow-delivery-policy");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let forbidden = FnTool::new(
        "forbidden",
        "must not run",
        json!({"type": "object"}),
        |_args, _ctx| async move { Ok(json!({"ran": true})) },
    );
    let agent = harness
        .agent(ScriptedModel::new(Vec::new()))
        .policy(ToolPolicy::new().deny(["forbidden"]))
        .tool(forbidden)
        .subagents(SubagentConfig::new().model(SubagentModel::new(
            "flash/child",
            "the child",
            Arc::new(Reporter) as Arc<dyn Model>,
        )))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let workflows = session.workflows().unwrap();
    route_to_child(&session.subagents().unwrap().settings());

    let ack = workflows
        .submit(WorkflowSubmission::new([stage("s", "try it", &[])]))
        .unwrap();
    wait_until(|| session.pending_completions() == 1, "the run outcome").await;
    let status = workflows.status(ack.run_id).unwrap();
    assert_eq!(status.state, RunState::Done);
    let outcome = status.outcome.unwrap();
    assert!(
        outcome.outputs["s"].contains("denied"),
        "the inherited policy denied the stage's call: {}",
        outcome.outputs["s"]
    );
    assert!(
        !outcome.outputs["s"].contains("\"ran\""),
        "the tool never ran"
    );

    let _ = std::fs::remove_dir_all(&root);
}
