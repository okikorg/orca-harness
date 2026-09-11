//! Detached subagent results on their way to the parent: delivered once,
//! at a model boundary, recorded with the transcript; observed by the
//! host without ever starting a run on the session's own initiative;
//! suppressed once the conversation that asked for them is gone.

use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::testing::ScriptedModel;
use orca_harness_core::{CancellationToken, FnTool, Message, Model, ModelResponse};
use orca_harness_sdk::orchestration::{BackgroundStatus, SubagentModel, SubagentRequest};
use orca_harness_sdk::{BackgroundNotification, Harness, RunRequest, SubagentConfig};
use serde_json::json;
use tokio::sync::Notify;

mod background_support;
mod common;
use background_support::{
    completion_messages, next_matching, route_to_child, routed_agent, spawn_call, tool_call,
    wait_until, Held,
};
use common::temp_dir;

#[tokio::test]
async fn completion_reaches_parent_once_at_next_model_call() {
    let root = temp_dir("delivery-once");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let parent = Arc::new(ScriptedModel::new(vec![
        spawn_call("c1", "look into it"),
        ModelResponse::final_text("spawned"),
        ModelResponse::final_text("turn two"),
        ModelResponse::final_text("turn three"),
    ]));
    let child = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text(
        "child done",
    )]));
    let agent = routed_agent(&harness, parent.clone(), child, SubagentConfig::new());
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    route_to_child(&subagents.settings());

    assert_eq!(session.run("delegate").await.unwrap().text, "spawned");
    wait_until(|| session.pending_completions() == 1, "the completion").await;
    assert!(
        completion_messages(&session.messages().await).is_empty(),
        "nothing is delivered between runs"
    );

    assert_eq!(session.run("next").await.unwrap().text, "turn two");
    let delivered = completion_messages(&session.messages().await);
    assert_eq!(delivered.len(), 1, "one batch, one user turn");
    assert!(delivered[0].contains("child done"), "{}", delivered[0]);
    let seen_by_turn_two = parent.observed_contexts()[2].clone();
    assert_eq!(
        completion_messages(seen_by_turn_two.messages()).len(),
        1,
        "the model saw the batch before answering turn two"
    );
    assert_eq!(session.pending_completions(), 0);

    assert_eq!(session.run("again").await.unwrap().text, "turn three");
    assert_eq!(
        completion_messages(&session.messages().await).len(),
        1,
        "a delivered batch is never delivered again"
    );
    assert_eq!(session.pending_completions(), 0);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn completion_during_run_is_delivered_before_next_model_step() {
    let root = temp_dir("delivery-mid-run");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let parent = Arc::new(ScriptedModel::new(vec![
        spawn_call("c1", "look into it"),
        tool_call("c2", "wait_for_child"),
        ModelResponse::final_text("folded in"),
    ]));
    let child = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text(
        "child done",
    )]));
    let ready = Arc::new(Notify::new());
    let wait_for_child = {
        let ready = ready.clone();
        FnTool::new(
            "wait_for_child",
            "waits until the child finished",
            json!({"type": "object"}),
            move |_args, _ctx| {
                let ready = ready.clone();
                async move {
                    ready.notified().await;
                    Ok(json!({"waited": true}))
                }
            },
        )
    };
    let agent = harness
        .agent(parent)
        .tool(wait_for_child)
        .subagents(SubagentConfig::new().model(SubagentModel::new(
            "flash/child",
            "the child",
            child,
        )))
        .build()
        .unwrap();
    let session = agent.new_session().persistent().open().unwrap();
    let id = session.id().unwrap();
    let subagents = session.subagents().unwrap();
    route_to_child(&subagents.settings());
    let mut notifications = session.notifications().unwrap();
    tokio::spawn({
        let ready = ready.clone();
        async move {
            next_matching(&mut notifications, "results ready", |n| {
                matches!(n, BackgroundNotification::CompletionsReady { .. }).then_some(())
            })
            .await;
            ready.notify_one();
        }
    });

    assert_eq!(
        session.run("delegate and wait").await.unwrap().text,
        "folded in"
    );
    let messages = session.messages().await;
    let wait_result = messages
        .iter()
        .position(|message| {
            matches!(message, Message::Tool { results } if results.iter().any(|r| r.call_id == "c2"))
        })
        .expect("the wait tool's result");
    let delivery = messages
        .iter()
        .position(|message| {
            matches!(message, Message::User { content, .. } if content.contains("background_subagent_completions"))
        })
        .expect("the delivered batch");
    let answer = messages
        .iter()
        .position(|message| {
            matches!(message, Message::Assistant { content: Some(text), .. } if text == "folded in")
        })
        .expect("the final answer");
    assert!(
        wait_result < delivery && delivery < answer,
        "delivery sits between the tool result and the next model step: {wait_result} < {delivery} < {answer}"
    );
    assert_eq!(session.pending_completions(), 0);
    drop(session);

    let resumed = agent.resume_session(&id).unwrap();
    assert_eq!(
        completion_messages(&resumed.messages().await).len(),
        1,
        "the delivered batch is part of the recorded transcript"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn idle_completion_notifies_host_without_launching_a_run() {
    let root = temp_dir("delivery-idle");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let parent = Arc::new(ScriptedModel::new(vec![
        spawn_call("c1", "look into it"),
        ModelResponse::final_text("spawned"),
        ModelResponse::final_text("took the results"),
    ]));
    let child = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text(
        "child done",
    )]));
    let agent = routed_agent(&harness, parent.clone(), child, SubagentConfig::new());
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    route_to_child(&subagents.settings());
    let mut notifications = session.notifications().unwrap();

    assert_eq!(session.run("delegate").await.unwrap().text, "spawned");
    let calls_after_first_run = parent.generate_calls();
    let spawn_id = subagents.active().first().map(|job| job.spawn.id);

    let finished = next_matching(&mut notifications, "the worker exit", |n| match n {
        BackgroundNotification::SubagentFinished(n) => Some(n),
        _ => None,
    })
    .await;
    assert_eq!(Some(finished.spawn.id), spawn_id);
    assert_eq!(finished.result.unwrap()["answer"], "child done");
    let pending = next_matching(&mut notifications, "results ready", |n| match n {
        BackgroundNotification::CompletionsReady { pending } => Some(pending),
        _ => None,
    })
    .await;
    assert_eq!(pending, 1);
    assert_eq!(session.pending_completions(), 1);

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        parent.generate_calls(),
        calls_after_first_run,
        "an idle completion never starts a model run"
    );
    assert!(completion_messages(&session.messages().await).is_empty());
    assert!(
        notifications.try_recv().is_err(),
        "nothing else was announced"
    );

    let result = session
        .continue_run(RunRequest::continuation())
        .await
        .unwrap();
    assert_eq!(result.text, "took the results");
    assert_eq!(completion_messages(&session.messages().await).len(), 1);
    assert_eq!(session.pending_completions(), 0);
    let delivered = next_matching(&mut notifications, "the delivery", |n| match n {
        BackgroundNotification::Delivered { spawn_ids } => Some(spawn_ids),
        _ => None,
    })
    .await;
    assert_eq!(delivered, vec![finished.spawn.id]);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn stale_generation_completions_are_suppressed() {
    let root = temp_dir("delivery-stale");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let parent = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text(
        "fresh turn",
    )]));
    let release = CancellationToken::new();
    let child: Arc<dyn Model> = Arc::new(Held(release.clone()));
    let agent = routed_agent(&harness, parent, child, SubagentConfig::new());
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    route_to_child(&subagents.settings());
    let mut notifications = session.notifications().unwrap();

    let ack = subagents.spawn(SubagentRequest::new("held")).unwrap();
    assert_eq!(ack.status, BackgroundStatus::Running);
    session.clear().await.unwrap();
    release.cancel();

    let finished = next_matching(&mut notifications, "the worker exit", |n| match n {
        BackgroundNotification::SubagentFinished(n) => Some(n),
        _ => None,
    })
    .await;
    assert_eq!(finished.spawn.id, ack.spawn_id);
    assert_eq!(finished.generation, 0, "the generation clear() replaced");
    assert_eq!(session.pending_completions(), 0);

    assert_eq!(session.run("hello").await.unwrap().text, "fresh turn");
    assert!(completion_messages(&session.messages().await).is_empty());
    while let Ok(notification) = notifications.try_recv() {
        assert!(
            !matches!(
                notification,
                BackgroundNotification::CompletionsReady { .. }
                    | BackgroundNotification::Delivered { .. }
            ),
            "a stale result is neither announced as ready nor delivered: {notification:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}
