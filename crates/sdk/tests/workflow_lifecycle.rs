//! Who owns a workflow run across the session lifecycle: shutdown settles
//! live runs and refuses later submissions, clear cancels runs and forgets
//! their outputs without delivering anything to the new conversation, and
//! a fork or a resume never shares a run, a store, or a spawn counter with
//! the session it came from.

use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::testing::ScriptedModel;
use orca_harness_core::{CancellationToken, ModelResponse};
use orca_harness_sdk::orchestration::RunState;
use orca_harness_sdk::{
    BackgroundNotification, Harness, SdkError, Stage, SubagentConfig, WorkflowSubmission,
};

mod background_support;
mod common;
use background_support::{
    completion_messages, next_matching, route_to_child, routed_agent, wait_until, Echo, Held,
};
use common::temp_dir;

fn stage(id: &str, prompt: &str) -> Stage {
    Stage::new(id, prompt)
}

#[tokio::test]
async fn shutdown_settles_live_runs_and_refuses_later_submissions() {
    let root = temp_dir("workflow-shutdown");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let parent = Arc::new(ScriptedModel::new(Vec::new()));
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
    let mut events = session.notifications();

    let ack = workflows
        .submit(WorkflowSubmission::new([stage("a", "held")]))
        .unwrap();
    assert_eq!(subagents.active().len(), 2, "the run and its stage");

    session.shutdown(Duration::from_secs(5)).await.unwrap();

    let settled = next_matching(&mut events, "the run's outcome", |event| match event {
        BackgroundNotification::SubagentFinished(n) if n.spawn.id == ack.run_id => Some(n),
        _ => None,
    })
    .await;
    assert!(settled.spawn.run.is_none());
    let error = settled.result.unwrap_err();
    assert!(error.starts_with("workflow cancelled"), "{error}");
    assert!(subagents.active().is_empty(), "the stage worker exited");
    assert!(workflows.runs().is_empty());
    assert!(
        workflows.status(ack.run_id).is_none(),
        "the close cleared the store with the runs"
    );
    assert_eq!(session.pending_completions(), 0, "nothing is owed");
    assert!(matches!(
        workflows.submit(WorkflowSubmission::new([stage("b", "later")])),
        Err(SdkError::SessionClosed)
    ));
    assert!(matches!(
        workflows.cancel(ack.run_id),
        Err(SdkError::SessionClosed)
    ));
    assert!(matches!(
        workflows.stage_output(ack.run_id, "a"),
        Err(SdkError::SessionClosed)
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn clear_cancels_running_workflows_and_delivers_nothing_to_the_new_conversation() {
    let root = temp_dir("workflow-clear");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let parent = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text("fresh")]));
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
    let mut events = session.notifications();

    let ack = workflows
        .submit(WorkflowSubmission::new([
            stage("a", "held"),
            stage("b", "held too"),
        ]))
        .unwrap();
    assert_eq!(workflows.status(ack.run_id).unwrap().active.len(), 2);

    session.clear().await.unwrap();

    let settled = next_matching(&mut events, "the run's outcome", |event| match event {
        BackgroundNotification::SubagentFinished(n) if n.spawn.id == ack.run_id => Some(n),
        _ => None,
    })
    .await;
    assert!(
        settled
            .result
            .unwrap_err()
            .starts_with("workflow cancelled"),
        "the run settled as cancelled"
    );
    assert!(workflows.runs().is_empty());
    assert!(
        workflows.status(ack.run_id).is_none(),
        "the store was cleared with the conversation"
    );
    wait_until(
        || subagents.active().is_empty(),
        "the stage workers to exit",
    )
    .await;
    assert_eq!(session.pending_completions(), 0);

    assert_eq!(session.run("hello").await.unwrap().text, "fresh");
    assert!(
        completion_messages(&session.messages().await).is_empty(),
        "a cancelled run's outcome never reaches the new conversation"
    );
    assert_eq!(session.pending_completions(), 0);

    // The session keeps serving: a new run is admitted and finishes.
    let again = workflows
        .submit(WorkflowSubmission::new([stage("c", "held again")]))
        .unwrap();
    assert!(again.run_id > ack.run_id, "one counter, never reused");
    assert_eq!(workflows.runs().len(), 1);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn fork_and_resume_start_with_no_workflow_runs() {
    let root = temp_dir("workflow-fork-resume");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(Echo)
        .subagents(SubagentConfig::default())
        .build()
        .unwrap();
    let session = agent.new_session().persistent().open().unwrap();
    let workflows = session.workflows().unwrap();
    let ack = workflows
        .submit(WorkflowSubmission::new([stage("a", "alpha")]))
        .unwrap();
    wait_until(|| session.pending_completions() == 1, "the run outcome").await;
    assert_eq!(workflows.status(ack.run_id).unwrap().state, RunState::Done);

    let fork = session.fork().await.unwrap();
    let forked = fork.workflows().expect("the fork has the same recipe");
    assert!(forked.runs().is_empty());
    assert!(
        forked.status(ack.run_id).is_none(),
        "the original's run is unknown to the fork"
    );
    assert!(matches!(
        forked.stage_output(ack.run_id, "a"),
        Err(SdkError::Workflow(message)) if message == "stage output is unavailable"
    ));
    assert_eq!(
        fork.pending_completions(),
        0,
        "the outcome is owed to the original alone"
    );

    // The fork's own run lives on its own counter and store: it may even
    // reuse the original's id without touching the original's outcome.
    let fork_ack = forked
        .submit(WorkflowSubmission::new([stage("a", "fork alpha")]))
        .unwrap();
    wait_until(|| fork.pending_completions() == 1, "the fork's outcome").await;
    assert_eq!(fork_ack.run_id, ack.run_id, "a fresh spawn counter");
    assert_eq!(
        forked.stage_output(fork_ack.run_id, "a").unwrap().answer,
        "fork alpha"
    );
    assert_eq!(
        workflows.stage_output(ack.run_id, "a").unwrap().answer,
        "alpha",
        "the original's store is untouched"
    );
    assert_eq!(session.pending_completions(), 1);

    let resumed = agent.resume_session(&session.id().unwrap()).unwrap();
    let resumed_workflows = resumed.workflows().unwrap();
    assert!(resumed_workflows.runs().is_empty());
    assert!(resumed_workflows.status(ack.run_id).is_none());
    assert_eq!(resumed.pending_completions(), 0);
    assert!(
        workflows.status(ack.run_id).is_some(),
        "the original still owns its run"
    );

    let _ = std::fs::remove_dir_all(&root);
}
