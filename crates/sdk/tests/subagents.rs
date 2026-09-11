//! Typed subagent operations through a session: foreground runs return a
//! typed outcome, detached spawns land in the session's inbox, and the
//! manager and inbox are session-owned (never shared, reset on clear).

use std::sync::Arc;

use orca_harness_core::testing::ScriptedModel;
use orca_harness_core::{CancellationToken, Model, ModelResponse};
use orca_harness_sdk::orchestration::{BackgroundStatus, SubagentRequest};
use orca_harness_sdk::{Harness, SdkError, SubagentConfig};

mod background_support;
mod common;
use background_support::{wait_until, Held};
use common::temp_dir;

#[tokio::test]
async fn session_subagents_run_foreground_returns_typed_outcome() {
    let root = temp_dir("subagents-foreground");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    // The parent never runs; the child consumes the script.
    let model = ScriptedModel::new(vec![ModelResponse::final_text("child done")]);
    let agent = harness
        .agent(model)
        .name("scripted")
        .subagents(SubagentConfig::default())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();

    let subagents = session.subagents().expect("configured subagents");
    let outcome = subagents
        .run(SubagentRequest::new("do the thing"), None, None)
        .await
        .unwrap();
    assert_eq!(outcome.answer, "child done");
    assert_eq!(outcome.steps, 1);
    assert_eq!(outcome.tool_calls, 0);
    let identity = outcome.identity.expect("inherited identity");
    assert_eq!(identity.provider, "sdk");
    assert_eq!(identity.model, "scripted");
    assert!(subagents.active().is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn session_without_subagents_has_no_handle() {
    let root = temp_dir("subagents-absent");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(ScriptedModel::new(Vec::new()))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    assert!(session.subagents().is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn session_subagents_spawn_background_and_inbox_receives_completion() {
    let root = temp_dir("subagents-background");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let release = CancellationToken::new();
    let agent = harness
        .agent(Held(release.clone()))
        .subagents(SubagentConfig::default())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();

    let ack = subagents.spawn(SubagentRequest::new("detached")).unwrap();
    assert_eq!(ack.status, BackgroundStatus::Running);
    let active = subagents.active();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].spawn.id, ack.spawn_id);
    assert_eq!(active[0].spawn.call_id, format!("host:{}", ack.spawn_id));
    assert_eq!(subagents.pending_completions(), 0);

    release.cancel();
    wait_until(|| subagents.pending_completions() == 1, "the completion").await;
    assert!(subagents.active().is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn host_spawns_queue_under_the_configured_background_limit() {
    let root = temp_dir("subagents-queue");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let release = CancellationToken::new();
    let agent = harness
        .agent(Held(release.clone()))
        .subagents(SubagentConfig::new().background_limit(1))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    assert_eq!(subagents.settings().background_limit(), 1);

    let first = subagents.spawn(SubagentRequest::new("one")).unwrap();
    let second = subagents.spawn(SubagentRequest::new("two")).unwrap();
    assert_eq!(first.status, BackgroundStatus::Running);
    assert_eq!(second.status, BackgroundStatus::Queued);

    assert!(subagents.cancel(second.spawn_id));
    release.cancel();
    wait_until(|| subagents.active().is_empty(), "both workers to finish").await;
    assert_eq!(subagents.pending_completions(), 2);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn host_spawns_follow_the_live_model_route() {
    let root = temp_dir("subagents-routing");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(ScriptedModel::new(Vec::new()))
        .subagents(SubagentConfig::default())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();

    let error = subagents
        .run(
            SubagentRequest::new("route").model("flash/nope"),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&error, SdkError::Subagent(message) if message.contains("unknown subagent model `flash/nope`")),
        "{error}"
    );
    assert!(subagents.active().is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn sessions_do_not_share_subagent_managers() {
    let root = temp_dir("subagents-isolated");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let release = CancellationToken::new();
    let agent = harness
        .agent(Held(release.clone()))
        .subagents(SubagentConfig::default())
        .build()
        .unwrap();
    let a = agent.new_session().ephemeral().open().unwrap();
    let b = agent.new_session().ephemeral().open().unwrap();
    let (a_subagents, b_subagents) = (a.subagents().unwrap(), b.subagents().unwrap());

    a_subagents.spawn(SubagentRequest::new("in a")).unwrap();
    assert_eq!(a_subagents.active().len(), 1);
    assert!(b_subagents.active().is_empty());
    assert_eq!(b_subagents.cancel_all(), 0);
    assert_eq!(a_subagents.active().len(), 1, "B cannot cancel A's jobs");

    assert_eq!(a_subagents.cancel_all(), 1);
    wait_until(|| a_subagents.active().is_empty(), "A's worker to stop").await;
    assert_eq!(
        a_subagents.pending_completions(),
        0,
        "cancel_all starts a new generation, so the cancelled result is dropped"
    );
    assert_eq!(b_subagents.pending_completions(), 0);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn clear_cancels_background_subagents() {
    let root = temp_dir("subagents-clear");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let release = CancellationToken::new();
    let agent = harness
        .agent(Held(release.clone()))
        .subagents(SubagentConfig::default())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();

    subagents.spawn(SubagentRequest::new("held")).unwrap();
    assert_eq!(subagents.active().len(), 1);
    session.clear().await.unwrap();
    wait_until(
        || subagents.active().is_empty(),
        "clear to cancel the worker",
    )
    .await;
    assert_eq!(subagents.pending_completions(), 0);

    // The handle survives clear and admits fresh work.
    let ack = subagents
        .spawn(SubagentRequest::new("after clear"))
        .unwrap();
    assert_eq!(ack.status, BackgroundStatus::Running);
    release.cancel();
    wait_until(
        || subagents.pending_completions() == 1,
        "the new completion",
    )
    .await;

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn model_tool_and_host_share_one_session_manager() {
    let root = temp_dir("subagents-shared-impl");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let release = CancellationToken::new();
    let held: Arc<dyn Model> = Arc::new(Held(release.clone()));
    // Parent turn: the model spawns a background subagent, then answers.
    let parent = ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![orca_harness_core::testing::call(
            "c1",
            "subagent",
            serde_json::json!({"task": "from the model", "background": true}),
        )]),
        ModelResponse::final_text("spawned"),
    ]);
    let agent = harness
        .agent(parent)
        .subagents(SubagentConfig::new().model(
            orca_harness_sdk::orchestration::SubagentModel::new("flash/held", "held worker", held),
        ))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    assert!(subagents
        .settings()
        .set_default_model(Some("flash/held".into())));

    assert_eq!(session.run("delegate").await.unwrap().text, "spawned");
    let active = subagents.active();
    assert_eq!(active.len(), 1, "the model's spawn is visible to the host");
    assert_eq!(active[0].spawn.call_id, "c1");
    assert_eq!(active[0].spawn.task, "from the model");

    let ack = subagents
        .spawn(SubagentRequest::new("from the host"))
        .unwrap();
    assert_eq!(ack.spawn_id, active[0].spawn.id + 1, "one id sequence");
    assert!(subagents.cancel(active[0].spawn.id));
    release.cancel();
    wait_until(|| subagents.active().is_empty(), "both workers").await;
    assert_eq!(subagents.pending_completions(), 2);

    let _ = std::fs::remove_dir_all(&root);
}
