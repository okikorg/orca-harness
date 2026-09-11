use super::*;
use async_trait::async_trait;
use orca_harness_core::{Context, Extension, ModelResponse};
use orca_harness_tools::{
    refresh_inventory, subagent_completions_prompt, ActiveInventory, SubagentManager, SubagentSpawn,
};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn notification(generation: u64, id: u64, answer: &str) -> SubagentNotification {
    SubagentNotification {
        generation,
        spawn: SubagentSpawn {
            stage: None,
            run: None,
            id,
            parent_id: None,
            depth: 0,
            call_id: format!("call-{id}"),
            task: format!("task {id}"),
            identity: None,
        },
        result: Ok(json!({"answer": answer})),
    }
}

#[tokio::test]
async fn interrupted_parent_answers_another_request_then_receives_background_results() {
    use orca_harness_core::testing::{call, ScriptedModel};
    use orca_harness_core::{Agent, CancellationToken, FnTool, Model, ModelError, ToolSchema};
    use orca_harness_tools::{SubagentTool, Workspace};
    use std::time::Duration;

    struct HeldChild(CancellationToken);
    #[async_trait]
    impl Model for HeldChild {
        async fn generate(
            &self,
            _: &Context,
            _: &[ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            self.0.cancelled().await;
            Ok(ModelResponse::final_text("child finished"))
        }
    }

    let release = CancellationToken::new();
    let manager = SubagentManager::new(1);
    let inbox = CompletionInbox::new(manager.clone());
    let (ui, mut ui_rx) = mpsc::unbounded_channel();
    let (worker, mut commands) = mpsc::unbounded_channel();
    let (done, mut received) = mpsc::unbounded_channel();
    let sink = inbox.clone();
    let notify_ui = ui.clone();
    let tool = SubagentTool::new(
        Arc::new(HeldChild(release.clone())),
        &Workspace::new(std::env::temp_dir()),
    )
    .inherited_identity("test", "held-worker")
    .background(manager.clone(), move |notification| {
        sink.publish(notification, &notify_ui, &worker);
        let _ = done.send(());
    });
    let parent_cancel = CancellationToken::new();
    let interrupt = parent_cancel.clone();
    let stop = FnTool::new(
        "interrupt",
        "interrupt the parent",
        json!({}),
        move |_, _| {
            let interrupt = interrupt.clone();
            async move {
                interrupt.cancel();
                Ok(json!({}))
            }
        },
    );
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![
            call(
                "child-1",
                "subagent",
                json!({"task": "first", "background": true}),
            ),
            call(
                "child-2",
                "subagent",
                json!({"task": "second", "background": true}),
            ),
        ]),
        ModelResponse::tool_calls(vec![call("legacy", "subagent", json!({"action": "wait"}))]),
        ModelResponse::final_text("Background work continues."),
        ModelResponse::tool_calls(vec![call("stop", "interrupt", json!({}))]),
        ModelResponse::final_text("Answer to the new request."),
        ModelResponse::final_text("Both children finished."),
    ]));
    let agent = Agent::new(model.clone())
        .tool(tool)
        .tool(stop)
        .extension(delivery(inbox.clone(), ui));
    let mut context = Context::new();
    context.push_user("delegate two tasks");
    let answer = tokio::time::timeout(
        Duration::from_secs(1),
        agent.run_context(&mut context, CancellationToken::new()),
    )
    .await
    .expect("parent must yield before children finish")
    .unwrap();
    assert_eq!(answer, "Background work continues.");
    context.push_user("do other work, then interrupt");
    assert!(agent
        .run_context(&mut context, parent_cancel)
        .await
        .is_err());
    super::super::context::repair_dangling_tool_calls(&mut context);
    assert_eq!(
        manager.active().len(),
        2,
        "running and queued children survive"
    );
    assert!(received.try_recv().is_err());
    context.push_user("a new request");
    let answer = tokio::time::timeout(
        Duration::from_secs(1),
        agent.run_context(&mut context, CancellationToken::new()),
    )
    .await
    .expect("new requests must not await children")
    .unwrap();
    assert_eq!(answer, "Answer to the new request.");
    assert_eq!(manager.active().len(), 2);
    let observed = model.observed_contexts();
    let snapshot = inventory_value(observed.last().unwrap());
    assert_eq!(snapshot["count"], 2);
    assert_eq!(snapshot["running"], 1);
    assert_eq!(snapshot["queued"], 1);
    assert_eq!(snapshot["agents"][0]["identity"]["model"], "held-worker");

    release.cancel();
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            ui_rx.recv().await,
            Some(UiMsg::SubagentCompleted {
                is_error: false,
                ..
            })
        ));
    }
    assert!(matches!(
        commands.try_recv(),
        Ok(WorkerCmd::BackgroundSubagentsReady)
    ));
    assert!(commands.try_recv().is_err(), "one coalesced wakeup");
    assert!(inbox.consume_wakeup());
    assert_eq!(
        agent
            .run_context(&mut context, CancellationToken::new())
            .await
            .unwrap(),
        "Both children finished."
    );
    assert!(!inbox.has_ready());
    assert_eq!(
        inventory_value(model.observed_contexts().last().unwrap())["count"],
        0
    );
}

fn inventory_value(context: &Context) -> serde_json::Value {
    context
        .messages()
        .iter()
        .rev()
        .find_map(|message| match message {
            orca_harness_core::Message::User { content, .. } => content
                .strip_prefix("Background agent inventory (host snapshot):\n")
                .map(|value| serde_json::from_str(value).unwrap()),
            _ => None,
        })
        .expect("model must receive the active inventory")
}

#[tokio::test]
async fn unchanged_inventory_reaches_model_when_same_call_compacts_the_old_snapshot() {
    use orca_harness_core::testing::ScriptedModel;
    use orca_harness_core::{Agent, CancellationToken, Model, ModelError, ToolSchema};
    use orca_harness_extensions::{ContextCapacity, LongSession, TruncationStore};
    use orca_harness_tools::{SubagentRequest, SubagentTool, Workspace};

    struct HeldChild(CancellationToken);
    #[async_trait]
    impl Model for HeldChild {
        async fn generate(
            &self,
            _: &Context,
            _: &[ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            self.0.cancelled().await;
            Ok(ModelResponse::final_text("child finished"))
        }
    }

    let manager = SubagentManager::new(1);
    let inbox = CompletionInbox::new(manager.clone());
    let (ui, _) = mpsc::unbounded_channel();
    // One live worker: an empty inventory is never restored, only a
    // non-empty one supersedes what compaction removed.
    let release = CancellationToken::new();
    let tool = SubagentTool::new(
        Arc::new(HeldChild(release.clone())),
        &Workspace::new(std::env::temp_dir()),
    )
    .background(manager.clone(), |_| {});
    tool.spawn_background(SubagentRequest::new("keep running"))
        .unwrap();
    let model = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text(
        "answer",
    )]));
    let compacted = Arc::new(AtomicBool::new(false));
    let observed = compacted.clone();
    // Match the interactive host: budget early delivery, compact, restore
    // the authoritative inventory before the actual model invocation.
    let agent = Agent::new(model.clone())
        .extension(delivery(inbox, ui))
        .extension(
            LongSession::new(
                ContextCapacity::new(Some(1000)),
                TruncationStore::new(100_000),
            )
            .on_compact(move |_| {
                observed.store(true, Ordering::Relaxed);
            }),
        )
        .extension(ActiveInventory::new(manager.clone()));
    let mut context = Context::new();
    context.push_user("original request");
    refresh_inventory(&mut context, &manager);
    context.push_assistant_text("old work ".repeat(2000));
    context.push_user("new request");
    agent
        .run_context(&mut context, CancellationToken::new())
        .await
        .unwrap();
    assert!(compacted.load(Ordering::Relaxed));
    let contexts = model.observed_contexts();
    assert_eq!(inventory_value(&contexts[0])["count"], 1);
    release.cancel();
    assert!(!contexts[0]
        .messages()
        .iter()
        .any(|message| matches!(message,
        orca_harness_core::Message::Assistant { content: Some(text), .. } if text.len() > 10000)));
}

#[test]
fn cancelled_generation_settles_ui_without_waking_parent() {
    let manager = SubagentManager::new(1);
    let inbox = CompletionInbox::new(manager.clone());
    let (ui, mut messages) = mpsc::unbounded_channel();
    let (worker, mut commands) = mpsc::unbounded_channel();
    manager.cancel_all();
    let mut cancelled = notification(0, 42, "");
    cancelled.result = Err("cancelled".into());
    inbox.publish(cancelled, &ui, &worker);
    assert!(matches!(
        messages.try_recv(),
        Ok(UiMsg::SubagentCompleted {
            id: 42,
            is_error: true,
            ..
        })
    ));
    assert!(commands.try_recv().is_err());
    assert!(!inbox.has_ready());
    inbox.publish(notification(1, 43, "current"), &ui, &worker);
    inbox.publish(notification(1, 44, "also current"), &ui, &worker);
    assert!(matches!(
        commands.try_recv(),
        Ok(WorkerCmd::BackgroundSubagentsReady)
    ));
    assert!(commands.try_recv().is_err());
    assert_eq!(inbox.drain().len(), 2);
}

#[test]
fn delivery_notice_names_every_spawn_in_the_batch() {
    let mut failed = notification(0, 5, "");
    failed.result = Err("subagent failed: boom".into());
    let batch = vec![notification(0, 4, "queue is ordered"), failed];
    assert_eq!(
        delivery_notice(&batch),
        "2 background agents reported · spawn 4, 5"
    );
    assert_eq!(
        delivery_notice(&batch[..1]),
        "1 background agent reported · spawn 4"
    );
}

/// A parent mid-turn gets results between its own steps: a tool that
/// runs while two workers finish is followed by one batch, before the
/// model's next call, not after the turn ends.
#[tokio::test]
async fn parent_mid_turn_receives_the_batch_before_its_next_model_call() {
    use orca_harness_core::testing::{call, ScriptedModel};
    use orca_harness_core::{Agent, FnTool, Message, Model};

    let inbox = CompletionInbox::new(SubagentManager::new(1));
    let (ui, _ui_rx) = mpsc::unbounded_channel();
    let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call("1", "probe", json!({}))]),
        ModelResponse::final_text("noted both results"),
    ]));
    let probe = {
        let inbox = inbox.clone();
        FnTool::new(
            "probe",
            "finishes two workers while the parent waits",
            json!({"type": "object"}),
            move |_, _| {
                let inbox = inbox.clone();
                async move {
                    inbox.push(notification(0, 8, "second"));
                    inbox.push(notification(0, 3, "first"));
                    Ok(json!({"ok": true}))
                }
            },
        )
    };
    let agent = Agent::new(model)
        .extension(delivery(inbox.clone(), ui))
        .tool(probe);
    let mut context = Context::new();
    context.push_user("orchestrate");

    let answer = agent
        .run_context(&mut context, orca_harness_core::CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(answer, "noted both results");
    let shape = context
        .messages()
        .iter()
        .map(|message| match message {
            Message::User { content, .. } if content.contains("background_subagent_inventory") => {
                "inventory"
            }
            Message::User { content, .. }
                if content.contains("background_subagent_completions") =>
            {
                "batch"
            }
            Message::User { .. } => "user",
            Message::Assistant { tool_calls, .. } if tool_calls.is_empty() => "answer",
            Message::Assistant { .. } => "calls",
            Message::Tool { .. } => "results",
            Message::System { .. } => "system",
        })
        .collect::<Vec<_>>();
    assert_eq!(shape, ["user", "calls", "results", "batch", "answer"]);
    let Message::User { content, .. } = &context.messages()[3] else {
        unreachable!()
    };
    let value: serde_json::Value = serde_json::from_str(content).unwrap();
    assert_eq!(value["count"], 2);
    assert_eq!(value["completions"][0]["spawnId"], 3);
    assert_eq!(value["completions"][1]["spawnId"], 8);
    assert!(!inbox.has_ready());
}

#[tokio::test]
async fn delivery_appends_one_user_turn_per_model_call_with_everything_ready() {
    let inbox = CompletionInbox::new(SubagentManager::new(1));
    let (ui, mut ui_rx) = mpsc::unbounded_channel();
    let delivery = delivery(inbox.clone(), ui);
    let mut context = Context::new();
    context.push_user("orchestrate");

    delivery.before_model(&mut context).await.unwrap();
    assert_eq!(
        context.messages().len(),
        1,
        "no inventory before anything was spawned"
    );

    inbox.push(notification(0, 2, "b"));
    inbox.push(notification(0, 1, "a"));
    delivery.before_model(&mut context).await.unwrap();
    assert_eq!(context.messages().len(), 2);
    let orca_harness_core::Message::User { content, .. } = &context.messages()[1] else {
        panic!("completions arrive as a user turn");
    };
    let value: serde_json::Value = serde_json::from_str(content).unwrap();
    assert_eq!(value["count"], 2);
    assert_eq!(value["completions"][0]["spawnId"], 1);
    assert_eq!(value["completions"][1]["spawnId"], 2);
    assert!(
        matches!(ui_rx.try_recv(), Ok(UiMsg::Notice(text)) if text.starts_with("2 background agents reported"))
    );
    assert!(!inbox.has_ready());
}

#[path = "tests/workflow.rs"]
mod workflow;
