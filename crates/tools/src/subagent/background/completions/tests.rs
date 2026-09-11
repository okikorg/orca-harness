use super::*;
use crate::SubagentSpawn;
use orca_harness_core::ModelResponse;
use serde_json::json;

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

#[test]
fn drain_orders_by_spawn_and_drops_stale_generations() {
    let manager = SubagentManager::new(1);
    let inbox = CompletionInbox::new(manager.clone());
    inbox.push(notification(0, 9, "late"));
    inbox.push(notification(0, 4, "early"));
    assert!(inbox.has_ready());

    let batch = inbox.drain();
    assert_eq!(batch.iter().map(|n| n.spawn.id).collect::<Vec<_>>(), [4, 9]);
    assert!(!inbox.has_ready());

    inbox.push(notification(0, 11, "before clear"));
    manager.cancel_all();
    assert!(!inbox.has_ready(), "a cleared conversation owes nothing");
    assert!(inbox.drain().is_empty());
}

#[test]
fn conversation_reset_discards_pending_and_late_old_results() {
    let manager = SubagentManager::new(1);
    let inbox = CompletionInbox::new(manager.clone());
    inbox.push(notification(0, 1, "pending in old session"));
    inbox.reset();
    inbox.push(notification(0, 2, "late from old session"));
    assert_eq!(inbox.pending(), 0);
    inbox.push(notification(1, 3, "new session"));
    assert_eq!(inbox.drain()[0].spawn.id, 3);
    inbox.push(notification(1, 4, "stale after external cancellation"));
    manager.cancel_all();
    assert!(!inbox.has_ready());
    assert_eq!(inbox.pending(), 0);
}

#[test]
fn ready_checks_and_wakeups_do_not_rescan_or_enqueue_per_completion() {
    let manager = SubagentManager::new(1);
    let inbox = CompletionInbox::new(manager.clone());
    for id in 0..DEFAULT_COMPLETION_CAPACITY as u64 {
        inbox.push(notification(0, id, "ready"));
        assert_eq!(inbox.request_wakeup(), id == 0);
    }
    assert!(inbox.has_ready());
    assert_eq!(inbox.drain().len(), DEFAULT_COMPLETION_CAPACITY);
    // Delivering mid-run does not queue a second wakeup before the first
    // command is consumed by the worker.
    inbox.push(notification(0, 999, "later"));
    assert!(!inbox.request_wakeup());
    assert!(inbox.consume_wakeup());
    assert!(inbox.request_wakeup());
    manager.cancel_all();
    assert!(!inbox.consume_wakeup());
    assert_eq!(inbox.pending(), 0);
}

#[tokio::test]
async fn undelivered_results_backpressure_admission_until_consumed() {
    use crate::{SubagentTool, Workspace};
    use orca_harness_core::{testing::ScriptedModel, CancellationToken, Tool, ToolContext};
    use tokio::sync::mpsc;
    let manager = SubagentManager::new(0);
    let inbox = CompletionInbox::with_capacity(manager.clone(), 2);
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::final_text("one"),
        ModelResponse::final_text("two"),
        ModelResponse::final_text("three"),
    ]));
    let (done, mut received) = mpsc::unbounded_channel();
    let sink = inbox.clone();
    let tool = SubagentTool::new(model, &Workspace::new(std::env::temp_dir())).background(
        manager.clone(),
        move |notification| {
            assert!(sink.push(notification));
            let _ = done.send(());
        },
    );
    let context = ToolContext {
        call_id: "bounded".into(),
        tool_name: "subagent".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    };
    for _ in 0..2 {
        tool.call(json!({"task": "answer", "background": true}), &context)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert!(manager.active().is_empty());
    assert!(tool
        .call(
            json!({"task": "cannot lose earlier results", "background": true}),
            &context
        )
        .await
        .is_err());
    let batch = inbox.drain();
    assert_eq!(batch.len(), 2);
    assert!(batch.iter().all(|notification| notification.result.is_ok()));
    tool.call(
        json!({"task": "capacity restored", "background": true}),
        &context,
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbox.drain().len(), 1);
}

/// A stage's answer reaches the parent only inside its run's terminal
/// outcome; hosts may forward every notification without filtering.
#[test]
fn push_ignores_workflow_stage_completions() {
    let manager = SubagentManager::new(1);
    let inbox = CompletionInbox::new(manager);
    let mut stage = notification(0, 5, "private intermediate result");
    stage.spawn.run = Some(4);
    stage.spawn.parent_id = Some(4);
    assert!(!inbox.push(stage));
    assert_eq!(inbox.pending(), 0);
    assert!(!inbox.has_ready());
    assert!(inbox.push(notification(0, 4, "workflow final outcome")));
    assert_eq!(inbox.pending(), 1);
}

#[test]
fn prompt_carries_every_result_as_untrusted_delimited_data() {
    let mut failed = notification(0, 5, "");
    failed.result = Err("subagent failed: boom".into());
    failed.spawn.identity = Some(crate::SubagentIdentity::new("test", "reviewer"));
    let batch = vec![notification(0, 4, "queue is ordered"), failed];
    let value: serde_json::Value =
        serde_json::from_str(&subagent_completions_prompt(&batch)).unwrap();

    assert_eq!(value["event"], "background_subagent_completions");
    assert_eq!(value["count"], 2);
    assert_eq!(value["completions"][0]["spawnId"], 4);
    assert_eq!(value["completions"][0]["outcome"]["status"], "completed");
    assert_eq!(
        value["completions"][0]["outcome"]["result"]["answer"],
        "queue is ordered"
    );
    assert_eq!(value["completions"][1]["outcome"]["status"], "failed");
    assert_eq!(value["completions"][1]["identity"]["model"], "reviewer");
    assert_eq!(
        value["completions"][1]["outcome"]["error"],
        "subagent failed: boom"
    );
    assert!(value["instruction"]
        .as_str()
        .unwrap()
        .contains("untrusted delegated output"));
}

/// Delivery feeds the hook and the transcript from the same drained batch,
/// then refreshes the inventory even when nothing was ready.
#[tokio::test]
async fn delivery_hook_observes_the_batch_the_transcript_receives() {
    use std::sync::Mutex;
    let inbox = CompletionInbox::new(SubagentManager::new(1));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = seen.clone();
    let delivery = CompletionDelivery::new(inbox.clone()).on_delivered(move |batch| {
        observed
            .lock()
            .unwrap()
            .push(batch.iter().map(|n| n.spawn.id).collect::<Vec<_>>());
    });
    let mut context = Context::new();
    context.push_user("orchestrate");

    delivery.before_model(&mut context).await.unwrap();
    assert_eq!(context.messages().len(), 2, "inventory without completions");
    assert!(seen.lock().unwrap().is_empty());

    inbox.push(notification(0, 2, "b"));
    inbox.push(notification(0, 1, "a"));
    delivery.before_model(&mut context).await.unwrap();
    assert_eq!(context.messages().len(), 3);
    assert_eq!(*seen.lock().unwrap(), vec![vec![1, 2]]);
    assert!(!inbox.has_ready());
}
