//! Detailed terminal outcomes and bounded event observation: a failed or
//! cancelled run still reports its partial usage and transcript, a
//! persistence failure is reported next to (not instead of) the run's own
//! result, and the observational event stream never blocks the run,
//! never grows without bound, and marks what it dropped.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    CancellationToken, FnTool, HarnessError, Limits, Message, ModelResponse, Usage,
};
use orca_harness_sdk::{Agent, Harness, HarnessEvent, RunEvent, RunOutcome, RunRequest, SdkError};
use serde_json::json;
use tokio::sync::Notify;

mod common;
use common::temp_dir;

fn usage(input_tokens: u64, output_tokens: u64) -> Usage {
    Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens: 0,
        cache_create_tokens: 0,
        reasoning_tokens: None,
    }
}

fn tool_step(id: &str, tool: &str, usage: Option<Usage>) -> ModelResponse {
    ModelResponse::ToolCalls {
        content: None,
        calls: vec![call(id, tool, json!({}))],
        usage,
    }
}

/// Does nothing, after `delay`: a run paced against a reader.
fn noop_tool(delay: Duration) -> FnTool {
    FnTool::new(
        "noop",
        "does nothing",
        json!({ "type": "object" }),
        move |_args, _ctx| async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            Ok(json!({ "ok": true }))
        },
    )
}

/// Sleeps far longer than any test deadline; `started` fires once the
/// tool body is running.
fn slow_tool(started: Arc<Notify>) -> FnTool {
    FnTool::new(
        "slow",
        "sleeps for a long time",
        json!({ "type": "object" }),
        move |_args, _ctx| {
            let started = started.clone();
            async move {
                started.notify_one();
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(json!({ "status": "done" }))
            }
        },
    )
}

/// An agent that calls `noop` `rounds` times, each taking `step_delay`,
/// and then answers `done`.
fn noop_agent(harness: &Harness, rounds: usize, step_delay: Duration) -> Agent {
    let mut script: Vec<ModelResponse> = (0..rounds)
        .map(|index| tool_step(&format!("n{index}"), "noop", None))
        .collect();
    script.push(ModelResponse::final_text("done"));
    harness
        .agent(ScriptedModel::new(script))
        .tool(noop_tool(step_delay))
        .build()
        .unwrap()
}

fn assert_execution_is<F>(outcome: &RunOutcome, predicate: F)
where
    F: Fn(&HarnessError) -> bool,
{
    match &outcome.execution {
        Err(SdkError::Harness(error)) if predicate(error) => {}
        other => panic!("unexpected execution outcome: {other:?}"),
    }
}

#[tokio::test]
async fn outcome_success_has_no_errors_and_matches_result() {
    let root = temp_dir("outcome-success");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(ScriptedModel::new(vec![ModelResponse::final_text("fine")]))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();

    let outcome = session.run_outcome("go").await.unwrap();
    assert!(outcome.is_success());
    assert_eq!(outcome.execution.as_deref().unwrap(), "fine");
    assert!(outcome.persistence.is_ok());
    assert_eq!(outcome.dropped_events, 0);
    let result = outcome.into_result().unwrap();
    assert_eq!(result.text, "fine");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn model_failure_keeps_partial_usage_and_transcript() {
    let root = temp_dir("outcome-model-failure");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    // One tool round with usage, then the script runs out: a model error.
    let model = ScriptedModel::new(vec![tool_step("c1", "noop", Some(usage(10, 5)))]);
    let agent = harness
        .agent(model)
        .tool(noop_tool(Duration::ZERO))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();

    let outcome = session.run_outcome("go").await.unwrap();
    assert_execution_is(&outcome, |error| matches!(error, HarnessError::Model(_)));
    assert!(!outcome.is_success());
    assert_eq!(outcome.usage.input_tokens, 10);
    assert_eq!(outcome.usage.output_tokens, 5);
    assert_eq!(outcome.metered_steps, 1);
    assert!(outcome.persistence.is_ok());
    assert!(outcome
        .messages
        .iter()
        .any(|message| matches!(message, Message::User { content, .. } if content == "go")));
    assert!(outcome.messages.iter().any(
        |message| matches!(message, Message::Assistant { tool_calls, .. } if tool_calls.len() == 1)
    ));
    assert!(outcome
        .messages
        .iter()
        .any(|message| matches!(message, Message::Tool { results } if results.len() == 1)));
    assert!(matches!(
        outcome.into_result(),
        Err(SdkError::Harness(HarnessError::Model(_)))
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn cancellation_keeps_partial_usage() {
    let root = temp_dir("outcome-cancelled");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let started = Arc::new(Notify::new());
    let model = ScriptedModel::new(vec![tool_step("s1", "slow", Some(usage(7, 3)))]);
    let agent = harness
        .agent(model)
        .tool(slow_tool(started.clone()))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();

    let token = CancellationToken::new();
    let handle = session
        .start(RunRequest::new("go").cancellation(token.clone()))
        .unwrap();
    started.notified().await;
    token.cancel();

    let outcome = handle.outcome().await.unwrap();
    assert_execution_is(&outcome, |error| matches!(error, HarnessError::Cancelled));
    assert_eq!(outcome.usage.input_tokens, 7);
    assert!(outcome.usage.input_tokens > 0);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn step_limit_and_deadline_report_partial_usage() {
    let root = temp_dir("outcome-limits");
    let harness = Harness::builder().workspace(&root).build().unwrap();

    let model = ScriptedModel::new(vec![
        tool_step("c1", "noop", Some(usage(11, 2))),
        ModelResponse::final_text("never reached"),
    ]);
    let agent = harness
        .agent(model)
        .tool(noop_tool(Duration::ZERO))
        .limits(Limits {
            max_steps: 1,
            ..Limits::default()
        })
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let outcome = session.run_outcome("go").await.unwrap();
    assert_execution_is(&outcome, |error| {
        matches!(error, HarnessError::StepLimitExceeded)
    });
    assert_eq!(outcome.usage.input_tokens, 11);

    let started = Arc::new(Notify::new());
    let model = ScriptedModel::new(vec![tool_step("s1", "slow", Some(usage(4, 1)))]);
    let agent = harness
        .agent(model)
        .tool(slow_tool(started))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let outcome = session
        .run_outcome(RunRequest::new("go").deadline(Duration::from_millis(50)))
        .await
        .unwrap();
    assert_execution_is(&outcome, |error| {
        matches!(error, HarnessError::DeadlineExceeded)
    });
    assert_eq!(outcome.usage.input_tokens, 4);

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn persistence_failure_is_reported_separately() {
    let root = temp_dir("outcome-persistence");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(ScriptedModel::new(vec![ModelResponse::final_text(
            "saved?",
        )]))
        .build()
        .unwrap();
    let session = agent.new_session().persistent().open().unwrap();

    // The recovery store is written next to the transcript; a directory in
    // its place makes that write fail while the transcript itself still
    // appends to its open handle.
    let recovery = session.path().unwrap().with_extension("recovery.json");
    let _ = std::fs::remove_file(&recovery);
    std::fs::create_dir_all(&recovery).unwrap();

    let outcome = session.run_outcome("go").await.unwrap();
    assert_eq!(outcome.execution.as_deref().unwrap(), "saved?");
    assert!(outcome.persistence.is_err(), "{:?}", outcome.persistence);
    assert!(!outcome.is_success());
    assert!(outcome.into_result().is_err());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn no_reader_does_not_block_finish_and_reports_overflow() {
    let root = temp_dir("events-no-reader");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = noop_agent(&harness, 6, Duration::ZERO);
    let session = agent.new_session().ephemeral().open().unwrap();

    let emitted = Arc::new(AtomicU64::new(0));
    let counter = emitted.clone();
    let request = RunRequest::new("go").event_capacity(2).on_event(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    let mut handle = session.start(request).unwrap();
    let mut receiver = handle.take_events().unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(5), handle.outcome())
        .await
        .expect("a full channel must not block the run")
        .unwrap();
    assert!(outcome.is_success());
    assert!(outcome.dropped_events > 0);

    let mut delivered = 0u64;
    let mut dropped = 0u64;
    let mut markers = 0u64;
    while let Some(event) = receiver.recv().await {
        match event {
            RunEvent::Harness(_) => delivered += 1,
            RunEvent::Overflow { dropped: gap } => {
                markers += 1;
                dropped += gap;
            }
        }
    }
    assert!(markers > 0);
    assert_eq!(dropped, outcome.dropped_events);
    assert_eq!(delivered + dropped, emitted.load(Ordering::SeqCst));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn slow_reader_sees_overflow_marker_in_order() {
    let root = temp_dir("events-slow-reader");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    // Each step bursts several events into a channel of two, then pauses
    // long enough for the reader to drain it: gaps and deliveries
    // interleave instead of the run finishing before the second recv.
    let agent = noop_agent(&harness, 6, Duration::from_millis(20));
    let session = agent.new_session().ephemeral().open().unwrap();

    let mut handle = session
        .start(RunRequest::new("go").event_capacity(2))
        .unwrap();
    let mut received = Vec::new();
    while let Some(event) = handle.events().recv().await {
        received.push(event);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let outcome = handle.outcome().await.unwrap();
    assert!(outcome.is_success());

    assert!(
        matches!(
            received.first(),
            Some(RunEvent::Harness(HarnessEvent::AgentStart))
        ),
        "first event: {:?}",
        received.first()
    );
    let first_marker = received
        .iter()
        .position(|event| matches!(event, RunEvent::Overflow { .. }))
        .expect("an overflow marker");
    assert!(first_marker > 0);
    assert!(
        first_marker < received.len() - 1,
        "marker precedes later events"
    );
    let marked: u64 = received
        .iter()
        .filter_map(|event| match event {
            RunEvent::Overflow { dropped } => Some(*dropped),
            RunEvent::Harness(_) => None,
        })
        .sum();
    assert_eq!(marked, outcome.dropped_events);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn dropped_reader_does_not_fail_run() {
    let root = temp_dir("events-dropped-reader");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = noop_agent(&harness, 6, Duration::ZERO);
    let session = agent.new_session().ephemeral().open().unwrap();

    let mut handle = session
        .start(RunRequest::new("go").event_capacity(2))
        .unwrap();
    drop(handle.take_events().unwrap());
    assert!(handle.take_events().is_none());
    // The replacement receiver is closed: it yields nothing and never waits.
    assert!(handle.events().recv().await.is_none());

    let outcome = tokio::time::timeout(Duration::from_secs(5), handle.outcome())
        .await
        .expect("a closed channel must not block the run")
        .unwrap();
    assert!(outcome.is_success());
    assert_eq!(outcome.dropped_events, 0);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn finish_without_draining_returns_result() {
    let root = temp_dir("events-finish-undrained");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = noop_agent(&harness, 2, Duration::ZERO);
    let session = agent.new_session().ephemeral().open().unwrap();

    let handle = session.start("go").unwrap();
    assert_eq!(handle.dropped_events(), 0);
    let result = tokio::time::timeout(Duration::from_secs(5), handle.finish())
        .await
        .expect("finish must not wait for a reader")
        .unwrap();
    assert_eq!(result.text, "done");

    let _ = std::fs::remove_dir_all(&root);
}
