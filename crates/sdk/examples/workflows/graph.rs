//! Workflows through a session: the host submits a dependency graph,
//! watches each stage finish, polls the run's status, and reads the
//! outcome and the stored stage outputs.
//!
//! A workflow is session-owned detached work like a subagent (its stages
//! are workers of the same manager); only the run-level outcome is owed
//! to the parent transcript. Compare `background_cancellation`, where a
//! `RunHandle` runs one parent turn in the background.
//!
//! Demonstrates:
//! 1. `Stage::new` with `needs`, a `{{ stages.<id>.output }}` template,
//!    a `string[]` source stage, and a `Kind::Map` stage over it using
//!    `{{ item }}`.
//! 2. `Workflows::submit(WorkflowSubmission)` and its acknowledgement.
//! 3. `BackgroundNotification::SubagentFinished` for every stage
//!    (`spawn.run` names the run, `spawn.stage` the stage).
//! 4. Polling `Workflows::status` until its outcome is recorded, then
//!    reading `WorkflowOutcome` outputs and per-stage `StageTiming`.
//! 5. `Workflows::stage_output` for a stored answer; the run-level
//!    `SubagentFinished` and `CompletionsReady` for the one completion
//!    owed to the parent; then `shutdown`.
//!
//! Deterministic: the stage model is scripted and never calls a provider.

#[path = "../support/mod.rs"]
mod support;

use std::time::Duration;

use async_trait::async_trait;
use orca_harness_sdk::orchestration::{RunState, StageStatus, StageTiming};
use orca_harness_sdk::{
    BackgroundNotification, Context, Harness, Kind, Message, Model, ModelError, ModelResponse,
    Stage, SubagentConfig, ToolSchema, WorkflowSubmission,
};
use support::TempWorkspace;

/// Answers the `dimensions` task with a JSON list and any other task with
/// the task text itself, so a stage's output is its rendered prompt.
struct Echo;

#[async_trait]
impl Model for Echo {
    async fn generate(&self, ctx: &Context, _: &[ToolSchema]) -> Result<ModelResponse, ModelError> {
        let task = ctx
            .messages()
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User { content, .. } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_default();
        Ok(ModelResponse::final_text(if task == "dimensions" {
            r#"["speed","safety"]"#.to_string()
        } else {
            task
        }))
    }
}

fn stage(id: &str, prompt: &str, needs: &[&str]) -> Stage {
    let mut stage = Stage::new(id, prompt);
    stage.needs = needs.iter().map(|need| need.to_string()).collect();
    stage
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempWorkspace::new("workflows");
    let harness = Harness::builder().workspace(workspace.path()).build()?;
    // The parent model is never called here; every stage runs on it.
    let agent = harness
        .agent(Echo)
        .subagents(SubagentConfig::default())
        .build()?;
    let session = agent.new_session().ephemeral().open()?;
    let workflows = session.workflows().expect("workflows are on by default");

    // Subscribe before submitting so no stage exit can slip past.
    let mut notifications = session.notifications();

    // a -> b (b reads a's output); source -> m (m fans out over the list).
    let mut source = stage("source", "dimensions", &[]);
    source.schema = Some("string[]".into());
    let mut map = stage("m", "check {{ item }}", &[]);
    map.kind = Kind::Map;
    map.over = Some("source".into());
    let ack = workflows.submit(
        WorkflowSubmission::new([
            stage("a", "summarize the change", &[]),
            stage("b", "review: {{ stages.a.output }}", &["a"]),
            source,
            map,
        ])
        .max_stages(16)
        .timeout(Duration::from_secs(30)),
    )?;
    println!("run {} admitted with {} stages", ack.run_id, ack.stages);
    assert_eq!(ack.stages, 4);

    // Every stage worker's exit is observable; mapped items appear as
    // `m[0]` and `m[1]`, while the map barrier `m` itself never runs.
    let mut finished = Vec::new();
    while finished.len() < 5 {
        let notification =
            tokio::time::timeout(Duration::from_secs(10), notifications.recv()).await??;
        if let BackgroundNotification::SubagentFinished(n) = notification {
            if n.spawn.run == Some(ack.run_id) {
                let name = n.spawn.stage.clone().expect("a stage name");
                println!(
                    "stage {name} finished -> {}",
                    n.result.expect("ok")["answer"]
                );
                finished.push(name);
            }
        }
    }
    finished.sort();
    assert_eq!(finished, ["a", "b", "m[0]", "m[1]", "source"]);

    // Poll the run until its stored outcome is present: the stage exits
    // above are announced before the run settles and records it.
    let mut status = workflows.status(ack.run_id).expect("a known run");
    for _ in 0..500 {
        if status.outcome.is_some() {
            break;
        }
        assert_eq!(status.state, RunState::Running);
        tokio::time::sleep(Duration::from_millis(10)).await;
        status = workflows.status(ack.run_id).expect("a known run");
    }
    println!("run {} is {:?}", ack.run_id, status.state);
    assert_eq!(status.state, RunState::Done);
    assert!(status.stages.values().all(|s| *s == StageStatus::Done));

    let outcome = status
        .outcome
        .expect("the run settled within the bounded wait");
    for (id, output) in &outcome.outputs {
        println!("output {id}: {output}");
    }
    assert_eq!(outcome.outputs["b"], "review: summarize the change");
    assert_eq!(outcome.outputs["m"], r#"["check speed","check safety"]"#);
    for (id, timing) in &outcome.timings {
        match timing {
            StageTiming::Ran { runtime_ms, cached } => {
                println!("timing {id}: ran in {runtime_ms:?} ms (cached: {cached})");
            }
            StageTiming::Skipped { virtual_stage } => {
                println!("timing {id}: skipped (map barrier: {virtual_stage})");
            }
        }
    }
    assert!(matches!(outcome.timings["a"], StageTiming::Ran { .. }));
    assert_eq!(
        outcome.timings["m"],
        StageTiming::Skipped {
            virtual_stage: true
        }
    );

    // Stored stage outputs stay readable after the run.
    let stored = workflows.stage_output(ack.run_id, "a")?;
    println!("stored output of {}: {}", stored.stage, stored.answer);
    assert_eq!(stored.answer, "summarize the change");

    // The run-level outcome is the one completion the parent is owed. It
    // is recorded before it is announced and filed, so wait for the
    // wake-up (`CompletionsReady`) rather than reading the inbox at once.
    loop {
        let notification =
            tokio::time::timeout(Duration::from_secs(10), notifications.recv()).await??;
        match notification {
            BackgroundNotification::SubagentFinished(n) if n.spawn.id == ack.run_id => {
                assert!(n.spawn.run.is_none(), "the run itself, not a stage");
                println!(
                    "run {} finished -> {:?}",
                    n.spawn.id,
                    n.result.map(|_| "ok")
                );
            }
            BackgroundNotification::CompletionsReady { pending } => {
                println!("completions ready: {pending}");
                break;
            }
            other => println!("other notification: {other:?}"),
        }
    }
    assert_eq!(session.pending_completions(), 1);
    session.shutdown(Duration::from_secs(2)).await?;
    println!("session shut down");
    Ok(())
}
