//! What a session's children run with and how a session lets go of them:
//! inherited policy, per-spawn host extensions and event relays, cleanup
//! on drop, per-spawn limits that leave the shared settings alone,
//! explicit bounded shutdown, and parent-run cancellation that leaves
//! detached children running.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    CancellationToken, Context, Extension, FnTool, HarnessError, Limits, Message, Model,
    ModelResponse,
};
use orca_harness_sdk::orchestration::{SubagentModel, SubagentRequest};
use orca_harness_sdk::{
    BackgroundNotification, Harness, HarnessEvent, RunRequest, SdkError, SubagentConfig,
    SubagentDepth, ToolPolicy,
};
use serde_json::json;
use tokio::sync::Notify;

mod background_support;
mod common;
use background_support::{
    next_matching, route_to_child, spawn_call, tool_call, wait_until, Held, Stall,
};
use common::temp_dir;

#[tokio::test]
async fn child_inherits_agent_policy() {
    let root = temp_dir("lifecycle-policy");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let forbidden = || {
        FnTool::new(
            "forbidden",
            "must not run",
            json!({"type": "object"}),
            |_args, _ctx| async move { Ok(json!({"ran": true})) },
        )
    };
    let denied_result = |context: &Context| -> Option<bool> {
        context.messages().iter().find_map(|message| match message {
            Message::Tool { results } if results[0].tool_name == "forbidden" => {
                Some(results[0].is_error && results[0].output.to_string().contains("denied"))
            }
            _ => None,
        })
    };

    let parent = Arc::new(ScriptedModel::tool_round(
        vec![call("p1", "forbidden", json!({}))],
        "parent finished",
    ));
    let child = Arc::new(ScriptedModel::tool_round(
        vec![call("f1", "forbidden", json!({}))],
        "child finished",
    ));
    let agent = harness
        .agent(parent)
        .policy(ToolPolicy::new().deny(["forbidden"]))
        .tool(forbidden())
        .subagents(SubagentConfig::new().model(SubagentModel::new(
            "flash/child",
            "the child",
            child.clone() as Arc<dyn Model>,
        )))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    route_to_child(&subagents.settings());

    let outcome = subagents
        .run(SubagentRequest::new("try it"), None, None)
        .await
        .unwrap();
    assert_eq!(outcome.answer, "child finished");
    assert_eq!(
        denied_result(&child.observed_contexts()[1]),
        Some(true),
        "the child's call was denied by the inherited policy"
    );
    assert_eq!(session.run("try it").await.unwrap().text, "parent finished");
    let mut parent_context = Context::new();
    for message in session.messages().await {
        parent_context.push(message);
    }
    assert_eq!(
        denied_result(&parent_context),
        Some(true),
        "the parent's own denial still holds"
    );

    let child = Arc::new(ScriptedModel::tool_round(
        vec![call("f2", "forbidden", json!({}))],
        "child finished",
    ));
    let agent = harness
        .agent(ScriptedModel::new(Vec::new()))
        .policy(ToolPolicy::new().deny(["forbidden"]))
        .tool(forbidden())
        .subagents(
            SubagentConfig::new()
                .inherit_extensions(false)
                .model(SubagentModel::new(
                    "flash/child",
                    "the child",
                    child.clone() as Arc<dyn Model>,
                )),
        )
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    route_to_child(&subagents.settings());
    subagents
        .run(SubagentRequest::new("try it"), None, None)
        .await
        .unwrap();
    assert_eq!(
        denied_result(&child.observed_contexts()[1]),
        Some(false),
        "without inheritance the child runs the tool"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn child_extension_factory_and_event_relay_run_per_spawn() {
    let root = temp_dir("lifecycle-child-extensions");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let started: Arc<Mutex<Vec<u64>>> = Arc::default();
    let config = SubagentConfig::new()
        .child_extensions({
            let factory_calls = factory_calls.clone();
            Arc::new(move |_spawn| {
                factory_calls.fetch_add(1, Ordering::SeqCst);
                Vec::new()
            })
        })
        .on_child_event({
            let started = started.clone();
            move |spawn, event| {
                if matches!(event, HarnessEvent::AgentStart) {
                    started.lock().unwrap().push(spawn.id);
                }
            }
        });
    // The parent never runs; the children consume the script.
    let agent = harness
        .agent(ScriptedModel::new(vec![
            ModelResponse::final_text("one"),
            ModelResponse::final_text("two"),
        ]))
        .subagents(config)
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();

    let first = subagents.spawn(SubagentRequest::new("first")).unwrap();
    wait_until(|| session.pending_completions() == 1, "the first child").await;
    let second = subagents.spawn(SubagentRequest::new("second")).unwrap();
    wait_until(|| session.pending_completions() == 2, "the second child").await;

    assert_eq!(factory_calls.load(Ordering::SeqCst), 2, "once per spawn");
    assert_eq!(
        *started.lock().unwrap(),
        vec![first.spawn_id, second.spawn_id],
        "the relay tags each child's events with its spawn"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn dropping_a_session_cancels_its_background_subagents() {
    let root = temp_dir("lifecycle-drop");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let release = CancellationToken::new();
    let agent = harness
        .agent(Held(release.clone()))
        .subagents(SubagentConfig::new())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    let mut notifications = session.notifications().unwrap();
    let ack = subagents.spawn(SubagentRequest::new("held")).unwrap();
    assert_eq!(subagents.active().len(), 1);

    drop(session);

    let finished = next_matching(&mut notifications, "the cancelled worker", |n| match n {
        BackgroundNotification::SubagentFinished(n) => Some(n),
        _ => None,
    })
    .await;
    assert_eq!(finished.spawn.id, ack.spawn_id);
    assert!(
        !release.is_cancelled(),
        "the worker exited on cancellation, not release"
    );
    assert!(subagents.active().is_empty());
    assert_eq!(subagents.pending_completions(), 0);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn explicit_limits_do_not_rewrite_shared_settings() {
    let root = temp_dir("lifecycle-limits");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let settings = SubagentDepth::default();
    let shared_steps = settings.max_steps();
    assert_ne!(shared_steps, 2);
    let noop = FnTool::new(
        "noop",
        "does nothing",
        json!({"type": "object"}),
        |_args, _ctx| async move { Ok(json!({})) },
    );
    // The parent never runs; the child consumes the script.
    let agent = harness
        .agent(ScriptedModel::new(vec![
            tool_call("n1", "noop"),
            tool_call("n2", "noop"),
            tool_call("n3", "noop"),
            ModelResponse::final_text("never reached"),
        ]))
        .tool(noop)
        .subagents(
            SubagentConfig::new()
                .settings(settings.clone())
                .limits(Limits {
                    max_steps: 2,
                    ..Limits::default()
                }),
        )
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let _second = agent.new_session().ephemeral().open().unwrap();
    assert_eq!(
        settings.max_steps(),
        shared_steps,
        "opening sessions leaves the shared step budget alone"
    );

    let error = session
        .subagents()
        .unwrap()
        .run(SubagentRequest::new("bounded"), None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, SdkError::Subagent(message) if message.contains("step limit")),
        "explicit limits still bound each spawn: {error}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn shutdown_awaits_workers_and_times_out_when_they_hang() {
    let root = temp_dir("lifecycle-shutdown");
    let harness = Harness::builder().workspace(&root).build().unwrap();

    let release = CancellationToken::new();
    let agent = harness
        .agent(Held(release.clone()))
        .subagents(SubagentConfig::new())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    subagents
        .spawn(SubagentRequest::new("cooperative"))
        .unwrap();
    assert_eq!(subagents.active().len(), 1);
    session.shutdown(Duration::from_secs(2)).await.unwrap();
    assert!(subagents.active().is_empty());
    assert!(!release.is_cancelled());

    let entered = Arc::new(Notify::new());
    let agent = harness
        .agent(Held(release.clone()))
        .subagents(SubagentConfig::new().child_extensions({
            let entered = entered.clone();
            Arc::new(move |_spawn| vec![Arc::new(Stall(entered.clone())) as Arc<dyn Extension>])
        }))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    subagents.spawn(SubagentRequest::new("stalled")).unwrap();
    entered.notified().await;
    let error = session
        .shutdown(Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(
        matches!(error, SdkError::ShutdownTimeout { still_active: 1 }),
        "{error}"
    );

    let session = harness
        .agent(ScriptedModel::new(Vec::new()))
        .build()
        .unwrap()
        .new_session()
        .ephemeral()
        .open()
        .unwrap();
    session.shutdown(Duration::ZERO).await.unwrap();

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn shutdown_while_a_run_is_active_touches_nothing() {
    let root = temp_dir("lifecycle-shutdown-busy");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let release = CancellationToken::new();
    let child: Arc<dyn Model> = Arc::new(Held(release.clone()));
    let slow_started = Arc::new(Notify::new());
    let slow = {
        let started = slow_started.clone();
        FnTool::new(
            "slow",
            "sleeps for a long time",
            json!({"type": "object"}),
            move |_args, _ctx| {
                let started = started.clone();
                async move {
                    started.notify_one();
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    Ok(json!({}))
                }
            },
        )
    };
    let agent = harness
        .agent(ScriptedModel::new(vec![
            tool_call("s1", "slow"),
            ModelResponse::final_text("never reached"),
        ]))
        .tool(slow)
        .subagents(SubagentConfig::new().model(SubagentModel::new(
            "flash/child",
            "the child",
            child,
        )))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    route_to_child(&subagents.settings());
    subagents.spawn(SubagentRequest::new("held")).unwrap();

    let handle = session.start(RunRequest::new("stall")).unwrap();
    slow_started.notified().await;
    let error = session.shutdown(Duration::ZERO).await.unwrap_err();
    assert!(matches!(error, SdkError::BusySession), "{error}");
    assert_eq!(
        subagents.active().len(),
        1,
        "a refused shutdown cancels nothing"
    );

    handle.cancellation_token().cancel();
    let _ = handle.finish().await;
    session.shutdown(Duration::from_secs(2)).await.unwrap();
    assert!(subagents.active().is_empty());
    assert!(!release.is_cancelled());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn cancelling_parent_run_does_not_cancel_detached_children() {
    let root = temp_dir("lifecycle-parent-cancel");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let parent = Arc::new(ScriptedModel::new(vec![
        spawn_call("c1", "look into it"),
        tool_call("s1", "slow"),
        ModelResponse::final_text("never reached"),
    ]));
    let release = CancellationToken::new();
    let child: Arc<dyn Model> = Arc::new(Held(release.clone()));
    let slow_started = Arc::new(Notify::new());
    let slow = {
        let started = slow_started.clone();
        FnTool::new(
            "slow",
            "sleeps for a long time",
            json!({"type": "object"}),
            move |_args, _ctx| {
                let started = started.clone();
                async move {
                    started.notify_one();
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    Ok(json!({}))
                }
            },
        )
    };
    let agent = harness
        .agent(parent)
        .tool(slow)
        .subagents(SubagentConfig::new().model(SubagentModel::new(
            "flash/child",
            "the child",
            child,
        )))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    route_to_child(&subagents.settings());

    let token = CancellationToken::new();
    let handle = session
        .start(RunRequest::new("delegate then stall").cancellation(token.clone()))
        .unwrap();
    slow_started.notified().await;
    assert_eq!(subagents.active().len(), 1);
    token.cancel();
    let error = handle.finish().await.unwrap_err();
    assert!(
        matches!(error, SdkError::Harness(HarnessError::Cancelled)),
        "{error}"
    );

    assert_eq!(
        subagents.active().len(),
        1,
        "the detached child outlives the parent run"
    );
    assert_eq!(subagents.cancel_all(), 1);
    wait_until(|| subagents.active().is_empty(), "the child to stop").await;

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn shutdown_closes_the_session_to_runs_and_spawns() {
    let root = temp_dir("lifecycle-closed");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let release = CancellationToken::new();
    let agent = harness
        .agent(Held(release.clone()))
        .subagents(SubagentConfig::new())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    subagents.spawn(SubagentRequest::new("before")).unwrap();
    session.shutdown(Duration::from_secs(2)).await.unwrap();

    let closed = |error: SdkError| assert!(matches!(error, SdkError::SessionClosed), "{error}");
    closed(subagents.spawn(SubagentRequest::new("after")).unwrap_err());
    closed(
        subagents
            .run(SubagentRequest::new("after"), None, None)
            .await
            .unwrap_err(),
    );
    closed(session.run("after").await.unwrap_err());
    closed(
        session
            .continue_run(RunRequest::continuation())
            .await
            .unwrap_err(),
    );
    closed(session.start(RunRequest::new("after")).err().unwrap());
    closed(session.clear().await.unwrap_err());
    closed(session.shutdown(Duration::ZERO).await.unwrap_err());
    assert!(subagents.active().is_empty());
    assert!(!release.is_cancelled());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn spawns_during_the_grace_window_are_refused() {
    let root = temp_dir("lifecycle-grace-window");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let release = CancellationToken::new();
    let entered = Arc::new(Notify::new());
    let agent = harness
        .agent(Held(release.clone()))
        .subagents(SubagentConfig::new().child_extensions({
            let entered = entered.clone();
            Arc::new(move |_spawn| vec![Arc::new(Stall(entered.clone())) as Arc<dyn Extension>])
        }))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let subagents = session.subagents().unwrap();
    subagents.spawn(SubagentRequest::new("stalled")).unwrap();
    entered.notified().await;

    let (shutdown, refused) = tokio::join!(session.shutdown(Duration::from_millis(300)), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        subagents.spawn(SubagentRequest::new("late")).unwrap_err()
    });
    assert!(matches!(refused, SdkError::SessionClosed), "{refused}");
    assert!(
        matches!(shutdown, Err(SdkError::ShutdownTimeout { still_active: 1 })),
        "only the stalled worker was ever live: {shutdown:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
