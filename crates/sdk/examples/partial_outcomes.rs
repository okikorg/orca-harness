//! Partial outcomes: what a run reports when it does not end with an
//! answer. `RunOutcome` keeps the usage, metered steps, and transcript of
//! the steps that did complete next to the reason the run stopped, and
//! the bounded event stream marks what it dropped instead of blocking.
//!
//! Demonstrates:
//! 1. `Session::run_outcome` on a model that fails after one tool step:
//!    `execution` is `Err`, usage and messages are retained.
//! 2. A caller-owned `RunRequest::cancellation` token, cancelled from
//!    inside a tool: `HarnessError::Cancelled` with partial usage.
//! 3. `RunRequest::deadline`: `HarnessError::DeadlineExceeded`.
//! 4. `RunRequest::event_capacity` with a reader that drains only after
//!    the run ended: `RunEvent::Overflow` markers sum to
//!    `RunOutcome::dropped_events`, and the run never waited.
//!
//! Deterministic: every model is scripted and never calls a provider.

mod support;

use std::time::Duration;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_sdk::{
    CancellationToken, FnTool, Harness, HarnessError, Message, ModelResponse, RunEvent, RunOutcome,
    RunRequest, SdkError, Usage,
};
use serde_json::json;
use support::TempWorkspace;

fn usage(input_tokens: u64, output_tokens: u64) -> Usage {
    Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens: 0,
        cache_create_tokens: 0,
        reasoning_tokens: None,
    }
}

fn tool_step(id: &str, tool: &str, usage: Usage) -> ModelResponse {
    ModelResponse::ToolCalls {
        content: None,
        calls: vec![call(id, tool, json!({}))],
        usage: Some(usage),
    }
}

fn noop_tool() -> FnTool {
    FnTool::new(
        "noop",
        "does nothing",
        json!({ "type": "object" }),
        |_args, _ctx| async move { Ok(json!({ "ok": true })) },
    )
}

/// Waits until the run's token is cancelled (by a caller or a deadline).
fn wait_tool() -> FnTool {
    FnTool::new(
        "wait",
        "waits for cancellation",
        json!({ "type": "object" }),
        |_args, ctx| async move {
            ctx.cancellation.cancelled().await;
            Ok(json!({ "status": "interrupted" }))
        },
    )
}

fn stop_reason(outcome: &RunOutcome) -> &HarnessError {
    match &outcome.execution {
        Err(SdkError::Harness(error)) => error,
        other => panic!("expected a harness error, got {other:?}"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempWorkspace::new("partial-outcomes");
    let harness = Harness::builder().workspace(workspace.path()).build()?;

    // 1. One tool round with usage, then the script runs out: a model error.
    println!("--- model failure after one step ---");
    let agent = harness
        .agent(ScriptedModel::new(vec![tool_step(
            "c1",
            "noop",
            usage(10, 5),
        )]))
        .tool(noop_tool())
        .build()?;
    let session = agent.new_session().ephemeral().open()?;
    let outcome = session.run_outcome("go").await?;
    println!(
        "stopped: {}; usage in={} out={}; metered steps={}; messages={}",
        stop_reason(&outcome),
        outcome.usage.input_tokens,
        outcome.usage.output_tokens,
        outcome.metered_steps,
        outcome.messages.len()
    );
    assert!(matches!(stop_reason(&outcome), HarnessError::Model(_)));
    assert!(!outcome.is_success());
    assert_eq!(outcome.usage.input_tokens, 10);
    assert_eq!(outcome.metered_steps, 1);
    assert!(outcome.persistence.is_ok());
    assert!(outcome
        .messages
        .iter()
        .any(|m| matches!(m, Message::Tool { results } if results.len() == 1)));
    assert!(outcome.into_result().is_err());

    // 2. A caller-owned token, cancelled from inside a tool.
    println!("--- cancellation ---");
    let token = CancellationToken::new();
    let cancel_tool = {
        let token = token.clone();
        FnTool::new(
            "cancel",
            "cancels the caller's token",
            json!({ "type": "object" }),
            move |_args, ctx| {
                let token = token.clone();
                async move {
                    token.cancel();
                    ctx.cancellation.cancelled().await;
                    Ok(json!({ "status": "cancelled" }))
                }
            },
        )
    };
    let agent = harness
        .agent(ScriptedModel::new(vec![tool_step(
            "c1",
            "cancel",
            usage(7, 3),
        )]))
        .tool(cancel_tool)
        .build()?;
    let session = agent.new_session().ephemeral().open()?;
    let outcome = session
        .run_outcome(RunRequest::new("go").cancellation(token.clone()))
        .await?;
    println!(
        "stopped: {}; usage in={}",
        stop_reason(&outcome),
        outcome.usage.input_tokens
    );
    assert!(matches!(stop_reason(&outcome), HarnessError::Cancelled));
    assert!(token.is_cancelled());
    assert_eq!(outcome.usage.input_tokens, 7);

    // 3. A deadline measured from the start of the run.
    println!("--- deadline ---");
    let agent = harness
        .agent(ScriptedModel::new(vec![tool_step(
            "c1",
            "wait",
            usage(4, 1),
        )]))
        .tool(wait_tool())
        .build()?;
    let session = agent.new_session().ephemeral().open()?;
    let outcome = session
        .run_outcome(RunRequest::new("go").deadline(Duration::from_millis(50)))
        .await?;
    println!(
        "stopped: {}; usage in={}",
        stop_reason(&outcome),
        outcome.usage.input_tokens
    );
    assert!(matches!(
        stop_reason(&outcome),
        HarnessError::DeadlineExceeded
    ));
    assert_eq!(outcome.usage.input_tokens, 4);

    // 4. Six tool rounds into a channel of two, drained only afterwards.
    println!("--- bounded event stream ---");
    let mut script: Vec<ModelResponse> = (0..6)
        .map(|i| tool_step(&format!("n{i}"), "noop", usage(1, 1)))
        .collect();
    script.push(ModelResponse::final_text("done"));
    let agent = harness
        .agent(ScriptedModel::new(script))
        .tool(noop_tool())
        .build()?;
    let session = agent.new_session().ephemeral().open()?;
    let mut handle = session.start(RunRequest::new("go").event_capacity(2))?;
    let mut events = handle.take_events().expect("the stream is taken once");
    let outcome = tokio::time::timeout(Duration::from_secs(5), handle.outcome()).await??;
    assert!(outcome.is_success(), "a full channel never blocks the run");
    let (mut delivered, mut dropped) = (0u64, 0u64);
    while let Some(event) = events.recv().await {
        match event {
            RunEvent::Harness(_) => delivered += 1,
            RunEvent::Overflow { dropped: gap } => {
                println!("overflow marker: {gap} events dropped");
                dropped += gap;
            }
        }
    }
    println!(
        "delivered {delivered}, dropped {dropped} (outcome reports {})",
        outcome.dropped_events
    );
    assert!(dropped > 0);
    assert_eq!(dropped, outcome.dropped_events);
    assert_eq!(outcome.into_result()?.text, "done");

    println!("all partial-outcome assertions passed");
    Ok(())
}
