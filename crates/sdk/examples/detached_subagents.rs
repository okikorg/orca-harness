//! Detached subagents through a session: the model delegates two tasks
//! to background workers, the host hears about each result on the
//! notification channel, and the results reach the parent transcript at
//! its next model call.
//!
//! Not to be confused with `background_cancellation`: a `RunHandle` runs
//! ONE parent turn in the background and hands its outcome back to the
//! caller. Subagents are independent child agents owned by the session;
//! they outlive the turn that spawned them and their results are owed to
//! the parent conversation, not to any handle.
//!
//! Demonstrates:
//! 1. `SubagentConfig` with a background concurrency limit and one
//!    host-approved child model (`flash/child`), selected as the default
//!    route through the live `Subagents::settings` handle.
//! 2. A parent turn that calls the `subagent` tool twice with
//!    `background: true` and answers at once.
//! 3. `Session::notifications()` delivering `SubagentFinished` per worker
//!    and one `CompletionsReady` wake-up; no run starts on its own.
//! 4. `Session::continue_run(RunRequest::continuation())` handing the
//!    batch to the parent as one `background_subagent_completions` turn,
//!    followed by `CompletionsDelivered`.
//! 5. A host-originated `Subagents::spawn` with its acknowledgement, then
//!    `Session::shutdown`.
//!
//! Deterministic: both models are scripted and never call a provider.

mod support;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_sdk::orchestration::{BackgroundStatus, SubagentModel, SubagentRequest};
use orca_harness_sdk::{
    BackgroundNotification, Context, Harness, Message, Model, ModelError, ModelResponse,
    RunRequest, SubagentConfig, ToolSchema,
};
use serde_json::json;
use support::TempWorkspace;

/// The child: answers every task with a one-line report about it.
struct Reporter;

#[async_trait]
impl Model for Reporter {
    async fn generate(&self, ctx: &Context, _: &[ToolSchema]) -> Result<ModelResponse, ModelError> {
        let task = ctx
            .messages()
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User { content, .. } => Some(content.clone()),
                _ => None,
            });
        Ok(ModelResponse::final_text(format!(
            "report: {}",
            task.unwrap_or_default()
        )))
    }
}

fn spawn_call(id: &str, task: &str) -> orca_harness_core::ToolCall {
    call(id, "subagent", json!({"task": task, "background": true}))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempWorkspace::new("detached-subagents");
    let harness = Harness::builder().workspace(workspace.path()).build()?;

    // Turn one spawns two workers and answers; the continuation answers
    // after the batch of results has been placed in front of the model.
    let parent = ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![
            spawn_call("c1", "summarize the diff"),
            spawn_call("c2", "list the risks"),
        ]),
        ModelResponse::final_text("two workers spawned"),
        ModelResponse::final_text("folded both reports in"),
    ]);
    let child: Arc<dyn Model> = Arc::new(Reporter);
    let agent = harness
        .agent(parent)
        .subagents(
            SubagentConfig::new()
                .background_limit(2)
                .model(SubagentModel::new(
                    "flash/child",
                    "scripted reporter",
                    child,
                )),
        )
        .build()?;
    let session = agent.new_session().ephemeral().open()?;
    let subagents = session.subagents().expect("subagents are configured");
    // Route every worker to the child model through the live settings.
    assert!(subagents
        .settings()
        .set_default_model(Some("flash/child".into())));

    // Subscribe before running so no notification can slip past.
    let mut notifications = session.notifications();

    let first = session.run("delegate the review").await?;
    println!("parent turn one: {}", first.text);
    assert_eq!(first.text, "two workers spawned");

    // Each worker's exit is observable; the results wait for the parent
    // and no run starts on the session's own initiative.
    for _ in 0..2 {
        let finished = next_matching(&mut notifications, |n| match n {
            BackgroundNotification::SubagentFinished(n) => Some(n),
            _ => None,
        })
        .await?;
        let answer = finished.result.expect("the worker succeeded");
        println!(
            "worker {} ({}) -> {}",
            finished.spawn.id, finished.spawn.task, answer["answer"]
        );
    }
    let pending = next_matching(&mut notifications, |n| match n {
        BackgroundNotification::CompletionsReady { pending } => Some(pending),
        _ => None,
    })
    .await?;
    println!(
        "completions ready: {pending} announced, {} pending",
        session.pending_completions()
    );
    assert_eq!(session.pending_completions(), 2);
    assert!(subagents.active().is_empty());

    // The host continues the conversation: the batch enters the transcript
    // as one user turn before the model answers.
    let second = session.continue_run(RunRequest::continuation()).await?;
    println!("parent continuation: {}", second.text);
    assert_eq!(second.text, "folded both reports in");
    assert_eq!(session.pending_completions(), 0);
    let batch = session
        .messages()
        .await
        .into_iter()
        .find_map(|message| match message {
            Message::User { content, .. }
                if content.contains("background_subagent_completions") =>
            {
                Some(content)
            }
            _ => None,
        })
        .expect("the delivered batch");
    println!("delivered batch:\n{batch}");
    assert!(batch.contains("report: summarize the diff"));
    assert!(batch.contains("report: list the risks"));
    let delivered = next_matching(&mut notifications, |n| match n {
        BackgroundNotification::CompletionsDelivered { spawn_ids } => Some(spawn_ids),
        _ => None,
    })
    .await?;
    assert_eq!(delivered.len(), 2);

    // The host may spawn workers itself, through the same manager.
    let ack = subagents.spawn(SubagentRequest::new("audit the tests"))?;
    println!("host spawn {} is {:?}", ack.spawn_id, ack.status);
    assert_eq!(ack.status, BackgroundStatus::Running);
    next_matching(&mut notifications, |n| match n {
        BackgroundNotification::SubagentFinished(n) if n.spawn.id == ack.spawn_id => Some(()),
        _ => None,
    })
    .await?;
    assert_eq!(session.pending_completions(), 1);

    session.shutdown(Duration::from_secs(2)).await?;
    println!("session shut down; the undelivered host result was dropped");
    Ok(())
}

/// The next notification `pick` accepts, within a bounded wait.
async fn next_matching<T>(
    receiver: &mut tokio::sync::broadcast::Receiver<BackgroundNotification>,
    mut pick: impl FnMut(BackgroundNotification) -> Option<T>,
) -> Result<T, Box<dyn std::error::Error>> {
    loop {
        let notification = tokio::time::timeout(Duration::from_secs(10), receiver.recv()).await??;
        if let Some(value) = pick(notification) {
            return Ok(value);
        }
    }
}
