//! Typed workflow operations through a session: a host submits a graph
//! without JSON, stage completions are observable but never owed to the
//! parent, only the run-level outcome awaits delivery, and the host and
//! the `workflow` model tool share one runtime and store.

use std::sync::Arc;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{CancellationToken, Message, ModelResponse};
use orca_harness_sdk::orchestration::{BackgroundStatus, RunState, StageStatus, SubagentRequest};
use orca_harness_sdk::{
    BackgroundNotification, Harness, SdkError, Stage, SubagentConfig, WorkflowSubmission,
};
use serde_json::{json, Value};

mod background_support;
mod common;
use background_support::{route_to_child, routed_agent, wait_until, Echo, Held};
use common::temp_dir;

fn stage(id: &str, prompt: &str, needs: &[&str]) -> Stage {
    let mut stage = Stage::new(id, prompt);
    stage.needs = needs.iter().map(|need| need.to_string()).collect();
    stage
}

#[tokio::test]
async fn session_workflows_submit_and_receive_terminal_outcome() {
    let root = temp_dir("workflows-terminal");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(Echo)
        .subagents(SubagentConfig::default())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let workflows = session.workflows().expect("workflows are on by default");
    let mut events = session.notifications();

    let ack = workflows
        .submit(WorkflowSubmission::new([
            stage("a", "alpha", &[]),
            stage("b", "beta {{ stages.a.output }}", &["a"]),
        ]))
        .unwrap();
    assert_eq!(ack.stages, 2);
    wait_until(|| session.pending_completions() == 1, "the run outcome").await;

    let mut stages = Vec::new();
    let mut run = None;
    let mut ready = 0;
    while let Ok(event) = events.try_recv() {
        match event {
            BackgroundNotification::SubagentFinished(n) if n.spawn.run == Some(ack.run_id) => {
                assert_eq!(n.spawn.call_id, "host:workflow");
                stages.push(n.spawn.stage.clone().expect("a stage name"));
            }
            BackgroundNotification::SubagentFinished(n) if n.spawn.id == ack.run_id => {
                assert!(n.spawn.run.is_none());
                run = Some(n.result.clone());
            }
            BackgroundNotification::CompletionsReady { pending } => {
                assert_eq!(pending, 1);
                ready += 1;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    stages.sort();
    assert_eq!(stages, ["a", "b"], "every stage completion was observable");
    let outcome = run.expect("the run-level completion").unwrap();
    assert_eq!(outcome["workflow"]["outputs"], json!({"b": "beta alpha"}));
    assert_eq!(ready, 1, "one wake-up, for the run outcome alone");
    assert_eq!(
        session.pending_completions(),
        1,
        "stage completions never await the parent"
    );

    let status = workflows
        .status(ack.run_id)
        .expect("a finished run is inspectable");
    assert_eq!(status.state, RunState::Done);
    assert_eq!(status.stages["a"], StageStatus::Done);
    assert_eq!(
        serde_json::to_value(&status.outcome.as_ref().unwrap().outputs).unwrap(),
        outcome["workflow"]["outputs"]
    );
    assert_eq!(
        workflows.stage_output(ack.run_id, "a").unwrap().answer,
        "alpha"
    );
    assert!(workflows.runs().is_empty());
    assert!(matches!(
        workflows.stage_output(ack.run_id, "nope").unwrap_err(),
        SdkError::Workflow(message) if message == "stage output is unavailable"
    ));
    assert!(matches!(
        workflows.cancel(ack.run_id).unwrap_err(),
        SdkError::Workflow(message) if message == "workflow is not running"
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn invalid_graphs_are_refused_before_admission() {
    let root = temp_dir("workflows-invalid");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(Echo)
        .subagents(SubagentConfig::default())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let workflows = session.workflows().unwrap();
    let error = workflows
        .submit(WorkflowSubmission::new([
            stage("a", "a", &["b"]),
            stage("b", "b", &["a"]),
        ]))
        .unwrap_err();
    assert!(
        matches!(&error, SdkError::Workflow(message) if message.contains("cycle involving")),
        "{error}"
    );
    assert!(workflows.runs().is_empty());
    assert!(session.subagents().unwrap().active().is_empty());
    assert_eq!(session.pending_completions(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn workflows_are_absent_without_subagents_or_when_disabled() {
    let root = temp_dir("workflows-absent");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let plain = harness.agent(Echo).build().unwrap();
    let session = plain.new_session().ephemeral().open().unwrap();
    assert!(session.subagents().is_none());
    assert!(session.workflows().is_none());

    let disabled = harness
        .agent(Echo)
        .subagents(SubagentConfig::new().workflows(false))
        .build()
        .unwrap();
    let session = disabled.new_session().ephemeral().open().unwrap();
    assert!(session.subagents().is_some());
    assert!(session.workflows().is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn model_tool_and_host_share_one_workflow_store() {
    let root = temp_dir("workflows-shared");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let release = CancellationToken::new();
    // Parent turn: the model lists workflows, then answers.
    let parent = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call("c1", "workflow", json!({"action": "list"}))]),
        ModelResponse::final_text("listed"),
    ]));
    let agent = routed_agent(
        &harness,
        parent,
        Arc::new(Held(release.clone())),
        SubagentConfig::new(),
    );
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    let workflows = session.workflows().unwrap();
    route_to_child(&subagents.settings());

    let ack = workflows
        .submit(WorkflowSubmission::new([stage("a", "held", &[])]))
        .unwrap();
    assert_eq!(
        session.run("what is running?").await.unwrap().text,
        "listed"
    );
    let listed = session
        .messages()
        .await
        .into_iter()
        .find_map(|message| match message {
            Message::Tool { results } => results
                .into_iter()
                .find(|result| result.tool_name == "workflow")
                .map(|result| result.output),
            _ => None,
        })
        .expect("the model's list result");
    assert_eq!(listed["runs"][0]["runId"], ack.run_id, "{listed}");
    assert_eq!(listed["runs"][0]["activeStages"][0]["status"], "running");

    let status = workflows
        .status(ack.run_id)
        .expect("the host sees the same run");
    assert_eq!(status.state, RunState::Running);
    assert_eq!(status.active.len(), 1);
    assert_eq!(status.active[0].stage.as_deref(), Some("a"));
    assert_eq!(status.active[0].status, BackgroundStatus::Running);
    assert_eq!(
        listed["runs"][0]["activeStages"][0]["spawnId"],
        status.active[0].spawn_id
    );
    assert_eq!(session.pending_completions(), 0);

    release.cancel();
    wait_until(|| session.pending_completions() == 1, "the run outcome").await;
    assert_eq!(
        workflows.stage_output(ack.run_id, "a").unwrap().answer,
        "held done"
    );
    assert_eq!(workflows.status(ack.run_id).unwrap().state, RunState::Done);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn clear_discards_workflow_outputs_with_the_conversation() {
    let root = temp_dir("workflows-clear");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(Echo)
        .subagents(SubagentConfig::default())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let workflows = session.workflows().unwrap();
    let ack = workflows
        .submit(WorkflowSubmission::new([stage("a", "alpha", &[])]))
        .unwrap();
    wait_until(|| session.pending_completions() == 1, "the run outcome").await;
    assert!(workflows.status(ack.run_id).is_some());

    session.clear().await.unwrap();
    assert!(
        workflows.status(ack.run_id).is_none(),
        "outputs never outlive the transcript that asked for them"
    );
    assert!(matches!(
        workflows
            .submit(WorkflowSubmission::new([stage("a", "alpha", &[])]).resume_from(ack.run_id))
            .unwrap_err(),
        SdkError::Workflow(message) if message == "resumeFrom run does not exist"
    ));
    let _ = std::fs::remove_dir_all(&root);
}

/// The parent's latest worker inventory snapshot, as the model saw it.
fn inventory(messages: &[Message]) -> Value {
    messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User { content, .. } if content.contains("background_subagent_inventory") => {
                let json = content.split_once('\n').expect("a prefixed snapshot").1;
                Some(serde_json::from_str(json).expect("a JSON snapshot"))
            }
            _ => None,
        })
        .expect("an inventory snapshot")
}

/// Workflows have no services of their own: a run and its stages are jobs
/// of the session's one subagent manager, numbered on its spawn counter,
/// queued by its concurrency limit, routed by its settings, hidden from the
/// parent's inventory as stages, and stored where the model tool stores.
#[tokio::test]
async fn workflows_share_the_session_subagent_services() {
    let root = temp_dir("workflows-shared-services");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let release = CancellationToken::new();
    let parent = Arc::new(ScriptedModel::new(vec![
        ModelResponse::final_text("noted"),
        ModelResponse::tool_calls(vec![call(
            "w1",
            "workflow",
            json!({"action": "run", "graph": [{"id": "m", "prompt": "model stage"}]}),
        )]),
        ModelResponse::final_text("submitted"),
    ]));
    let agent = routed_agent(
        &harness,
        parent,
        Arc::new(Held(release.clone())),
        SubagentConfig::new().background_limit(1),
    );
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    let workflows = session.workflows().unwrap();
    route_to_child(&subagents.settings());

    // One spawn counter: the run, its two stages, then the plain worker.
    let ack = workflows
        .submit(WorkflowSubmission::new([
            stage("a", "held", &[]),
            stage("b", "held too", &[]),
        ]))
        .unwrap();
    let worker = subagents
        .spawn(SubagentRequest::new("plain worker"))
        .unwrap();
    assert_eq!(worker.spawn_id, ack.run_id + 3);

    // One manager: the host's full view holds the run, its stages, and
    // the worker, in that order.
    let active = subagents.active();
    let ids: Vec<u64> = active.iter().map(|job| job.spawn.id).collect();
    assert_eq!(
        ids,
        vec![ack.run_id, ack.run_id + 1, ack.run_id + 2, worker.spawn_id]
    );
    assert_eq!(
        active
            .iter()
            .filter(|job| job.spawn.run == Some(ack.run_id))
            .count(),
        2,
        "the stages are jobs of the run"
    );

    // One concurrency limit: `background_limit(1)` lets the first stage
    // run and queues the second stage and the worker behind it.
    let status = workflows.status(ack.run_id).unwrap();
    let by_stage = |name: &str| {
        status
            .active
            .iter()
            .find(|job| job.stage.as_deref() == Some(name))
            .unwrap()
            .status
    };
    assert_eq!(by_stage("a"), BackgroundStatus::Running);
    assert_eq!(by_stage("b"), BackgroundStatus::Queued);
    assert_eq!(worker.status, BackgroundStatus::Queued);

    // One route: the same rejection on both paths.
    let mut routed = stage("r", "routed", &[]);
    routed.model = Some("nope".into());
    let via_workflow = workflows
        .submit(WorkflowSubmission::new([routed]))
        .unwrap_err();
    let via_subagent = subagents
        .spawn(SubagentRequest::new("routed").model("nope"))
        .unwrap_err();
    match (via_workflow, via_subagent) {
        (SdkError::Workflow(workflow), SdkError::Subagent(subagent)) => {
            assert_eq!(workflow, "unknown subagent model `nope`");
            assert_eq!(subagent, workflow);
        }
        other => panic!("unexpected {other:?}"),
    }

    // The parent's inventory is the manager's parent view: the worker
    // alone, never a run or its stages.
    assert_eq!(session.run("what is running?").await.unwrap().text, "noted");
    let snapshot = inventory(&session.messages().await);
    assert_eq!(snapshot["count"], 1, "{snapshot}");
    assert_eq!(snapshot["agents"][0]["spawnId"], worker.spawn_id);

    // One store: the host reads what a model-submitted run wrote.
    release.cancel();
    wait_until(
        || session.pending_completions() == 2,
        "the run outcome and the worker",
    )
    .await;
    assert_eq!(session.run("delegate").await.unwrap().text, "submitted");
    let model_run = session
        .messages()
        .await
        .into_iter()
        .find_map(|message| match message {
            Message::Tool { results } => results
                .into_iter()
                .find(|result| result.tool_name == "workflow")
                .and_then(|result| result.output["runId"].as_u64()),
            _ => None,
        })
        .expect("the model's acknowledgement");
    wait_until(|| session.pending_completions() == 1, "the model's run").await;
    assert_eq!(
        workflows.stage_output(model_run, "m").unwrap().answer,
        "held done"
    );
    assert_eq!(workflows.status(model_run).unwrap().state, RunState::Done);
    assert_eq!(
        workflows.stage_output(ack.run_id, "b").unwrap().answer,
        "held done"
    );

    let _ = std::fs::remove_dir_all(&root);
}
