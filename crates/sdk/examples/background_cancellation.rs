//! Background runs via `RunHandle`, cancellation reaching a cooperative tool,
//! `BusySession` rejections, and completion behavior.
//!
//! Demonstrates:
//! 1. Starting a background agent run via `Session::start()`.
//! 2. Consuming real-time lifecycle events from `RunHandle::events()` (a
//!    bounded, observational stream: `RunEvent::Overflow` marks any gap).
//! 3. Verifying that a concurrent `run()` or `start()` is rejected with `SdkError::BusySession`.
//! 4. Cancelling a long-running tool via `RunHandle::cancellation_token()`:
//!    the kernel stops the tool at the token, so the run ends `Cancelled`
//!    and the tool never runs to completion. A cooperative tool may or
//!    may not observe the token itself before the kernel drops it.
//! 5. Verifying successful completion behavior on subsequent non-cancelled background runs.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{FnTool, HarnessError};
use orca_harness_extensions::HarnessEvent;
use orca_harness_sdk::{Harness, RunEvent, SdkError};
use serde_json::json;
use support::TempWorkspace;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempWorkspace::new("background-cancellation");
    println!(
        "Initializing Harness in workspace: {}",
        workspace.path().display()
    );

    let harness = Harness::builder().workspace(workspace.path()).build()?;

    // -------------------------------------------------------------------------
    // Scenario A: Cooperative Cancellation & BusySession Rejection
    // -------------------------------------------------------------------------
    println!("\n--- Scenario A: Cancellation and BusySession Rejection ---");

    let saw_cancellation = Arc::new(AtomicBool::new(false));
    let saw_cancellation_tool = saw_cancellation.clone();
    let completed = Arc::new(AtomicBool::new(false));
    let completed_tool = completed.clone();

    let notify_started = Arc::new(tokio::sync::Notify::new());
    let notify_started_tool = notify_started.clone();

    // A cooperative tool that waits for cancellation or completion
    let cancellable_tool = FnTool::new(
        "long_computation",
        "Performs a long-running cancellable computation",
        json!({ "type": "object" }),
        move |_args, ctx| {
            let saw = saw_cancellation_tool.clone();
            let completed = completed_tool.clone();
            let started = notify_started_tool.clone();
            async move {
                started.notify_one();
                println!("  [Tool] Computation started, awaiting cancellation or timeout...");
                // Loop with quick yields so cooperatively checking the token or select detects it
                for _ in 0..1000 {
                    if ctx.cancellation.is_cancelled() {
                        saw.store(true, Ordering::SeqCst);
                        println!("  [Tool] Cancellation token observed inside tool context!");
                        return Ok(json!({ "status": "cancelled" }));
                    }
                    tokio::task::yield_now().await;
                }
                completed.store(true, Ordering::SeqCst);
                Ok(json!({ "status": "completed" }))
            }
        },
    );

    let model_cancel = ScriptedModel::tool_round(
        vec![call("c1", "long_computation", json!({}))],
        "Done computation",
    );

    let agent_cancel = harness
        .agent(model_cancel)
        .name("background-agent")
        .tool(cancellable_tool)
        .events(true)
        .build()?;

    let session = agent_cancel.new_session().ephemeral().open()?;

    // 1. Start background execution
    println!("Starting background run via session.start()...");
    let mut handle = session.start("Execute long computation")?;

    // 2. While the run is in-flight, test BusySession rejection
    println!("Testing BusySession rejection on concurrent run & start...");
    let concurrent_run_err = session.run("Concurrent request").await;
    assert!(
        matches!(concurrent_run_err, Err(SdkError::BusySession)),
        "Expected BusySession error on concurrent run(), got: {:?}",
        concurrent_run_err
    );
    println!("  -> Correctly rejected concurrent session.run() with BusySession");

    let concurrent_start_err = session.start("Another request");
    assert!(
        matches!(concurrent_start_err, Err(SdkError::BusySession)),
        "Expected BusySession error on concurrent start()"
    );
    println!("  -> Correctly rejected concurrent session.start() with BusySession");

    // 3. Wait until the cooperative tool has started executing
    notify_started.notified().await;

    // 4. Signal cancellation
    println!("Cancelling run via handle.cancellation_token().cancel()...");
    handle.cancellation_token().cancel();

    // 5. Drain and inspect background events
    let mut received_events = Vec::new();
    while let Ok(event) =
        tokio::time::timeout(Duration::from_millis(50), handle.events().recv()).await
    {
        if let Some(ev) = event {
            println!("  [Event Stream] {:?}", ev);
            received_events.push(ev);
        } else {
            break;
        }
    }
    assert_eq!(handle.dropped_events(), 0, "a drained stream loses nothing");

    // 6. Await finish and verify cancellation error
    let finish_result = handle.finish().await;
    println!("Run result after cancellation: {:?}", finish_result);
    assert!(
        matches!(
            finish_result,
            Err(SdkError::Harness(HarnessError::Cancelled))
        ),
        "Expected HarnessError::Cancelled, got: {:?}",
        finish_result
    );
    assert!(
        !completed.load(Ordering::SeqCst),
        "a cancelled tool must not run to completion"
    );
    // Informational only: the kernel's dispatcher prefers the token, so the
    // tool future is usually dropped before its loop sees the cancellation.
    println!(
        "Tool observed the token itself: {}",
        saw_cancellation.load(Ordering::SeqCst)
    );

    // -------------------------------------------------------------------------
    // Scenario B: Normal Background Completion
    // -------------------------------------------------------------------------
    println!("\n--- Scenario B: Normal Background Completion ---");

    let fast_tool = FnTool::new(
        "fast_task",
        "Quick computation",
        json!({ "type": "object" }),
        |_args, _ctx| async move { Ok(json!({ "result": "fast_done" })) },
    );

    let model_complete = ScriptedModel::tool_round(
        vec![call("c2", "fast_task", json!({}))],
        "Task finished successfully.",
    );

    let agent_complete = harness
        .agent(model_complete)
        .name("fast-agent")
        .tool(fast_tool)
        .events(true)
        .build()?;

    let session_complete = agent_complete.new_session().ephemeral().open()?;

    println!("Starting non-cancelled background run...");
    let mut handle_complete = session_complete.start("Run fast task")?;

    // Drain events as they arrive before finishing the handle
    let mut completion_events = Vec::new();
    while let Ok(event) =
        tokio::time::timeout(Duration::from_millis(50), handle_complete.events().recv()).await
    {
        match event {
            Some(RunEvent::Harness(ev)) => completion_events.push(ev),
            Some(RunEvent::Overflow { dropped }) => println!("  [Event Stream] dropped {dropped}"),
            None => break,
        }
    }

    let run_res = handle_complete
        .finish()
        .await
        .expect("Background run should succeed");
    println!("Background run completed: {}", run_res.text);
    assert_eq!(run_res.text, "Task finished successfully.");

    // Verify events were emitted properly
    let has_tool_event = completion_events.iter().any(|e| {
        matches!(
            e,
            HarnessEvent::ToolCall { tool_name, .. } if tool_name == "fast_task"
        )
    });
    let has_result_event = completion_events.iter().any(|e| {
        matches!(
            e,
            HarnessEvent::Result { message } if message == "Task finished successfully."
        )
    });
    assert!(has_tool_event, "fast_task ToolCall event expected");
    assert!(has_result_event, "Result event expected");

    println!("\nAll background and cancellation assertions passed successfully!");
    Ok(())
}
