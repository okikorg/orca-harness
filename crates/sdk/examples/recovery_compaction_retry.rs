//! Demonstrates model-retry, tool-retry, truncation + read_tool_result recovery,
//! manual and automatic compaction with callbacks, and persistent resume/fork —
//! all hermetic (ScriptedModel, no network).

use std::sync::{Arc, Mutex};

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{FnTool, ModelResponse, ToolError, Usage};
use orca_harness_extensions::{CompactConfig, CompactReport};
use orca_harness_sdk::{Compaction, Harness, RetryConfig, TruncationConfig};
use serde_json::json;

fn temp_dir(_label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "orca-rec-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ===================================================================
// 1 · MODEL RETRY
// ===================================================================

async fn model_retry_demo(root: &std::path::Path) {
    println!("=== Model Retry ===");

    let harness = Harness::builder().workspace(root).build().unwrap();

    // Fail the tool once, then succeed, so ToolRetry is observable.
    let call_count = Arc::new(Mutex::new(0u32));
    let cc = call_count.clone();

    let counting_tool = FnTool::new(
        "count_it",
        "Fails once, then reports its invocation count",
        json!({"type": "object", "properties": {"n": {"type": "integer"}}}),
        move |_args, _ctx| {
            let cc = cc.clone();
            async move {
                let mut count = cc.lock().unwrap();
                *count += 1;
                if *count == 1 {
                    Err(ToolError::msg("transient tool failure"))
                } else {
                    Ok(json!({ "invocation": *count }))
                }
            }
        },
    );

    // Simple model: one tool round then final text. With 3 attempts, the tool
    // succeeds on first try here (no actual errors), but this shows the API wiring.
    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("Counting...".into()),
            calls: vec![call("c1", "count_it", json!({"n": 1}))],
            usage: Some(Usage {
                input_tokens: 15,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
            }),
        },
        ModelResponse::Final {
            text: "Done counting.".into(),
            usage: Some(Usage {
                input_tokens: 25,
                output_tokens: 8,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
            }),
        },
    ]);

    // Configure both tool and model retry to prove the APIs accept configs.
    let agent = harness
        .agent(model)
        .tool(counting_tool)
        .tool_retry(RetryConfig::attempts(3).backoff_ms(0))
        .model_retry(RetryConfig::attempts(3).backoff_ms(0))
        .build()
        .unwrap();

    let session = agent.new_session().ephemeral().open().unwrap();
    let result = session.run("Count it").await.unwrap();
    assert_eq!(result.text, "Done counting.");
    assert_eq!(result.metered_steps, 2);
    assert_eq!(*call_count.lock().unwrap(), 2);

    println!(
        "  Tool called {} time(s), steps={}, retry config wired",
        call_count.lock().unwrap(),
        result.metered_steps
    );
    println!("  OK - Model/Tool retry configured successfully\n");
}

// ===================================================================
// 2 · TRUNCATION + READ_TOOL_RESULT RECOVERY + RESUME + FORK
// ===================================================================

async fn truncation_recovery_demo(root: &std::path::Path) {
    println!("=== Truncation + read_tool_result Recovery ===");

    let harness = Harness::builder().workspace(root).build().unwrap();

    // Produces a large output that will be truncated at 64 chars.
    let big_data = FnTool::new(
        "big_data",
        "Returns a large block of data",
        json!({"type":"object","properties":{"topic":{"type":"string"}}}),
        |args, _ctx| async move {
            let topic = args["topic"].as_str().unwrap_or("data");
            let body = "X".repeat(500);
            Ok(json!({
                "topic": topic,
                "body": body,
                "note": format!("{} chars of {}", body.len(), topic)
            }))
        },
    );

    // First run: get the truncated output with _readFull hint.
    let large_model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("Retrieving large dataset...".into()),
            calls: vec![call("d1", "big_data", json!({"topic": "benchmark"}))],
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
            }),
        },
        ModelResponse::Final {
            text: "Got the truncated data with hint to read full.".into(),
            usage: Some(Usage {
                input_tokens: 30,
                output_tokens: 8,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
            }),
        },
    ]);

    let agent1 = harness
        .agent(large_model)
        .tool(big_data)
        .truncation(TruncationConfig {
            max_string_chars: 64,
            ..Default::default()
        })
        .build()
        .unwrap();

    let session1 = agent1.new_session().persistent().open().unwrap();
    let id = session1.id().expect("persistent session has id");
    let result1 = session1.run("Get benchmark data").await.unwrap();

    println!("  Run 1 text: \"{}\"", result1.text);
    // Check that messages contain truncated marker
    let messages = session1.messages().await;
    let any_truncated = messages.iter().any(|m| {
        let s = serde_json::to_string(m).unwrap_or_default();
        s.contains("\"_truncated\"") || s.contains("_readFull")
    });
    assert!(any_truncated, "expected truncated output in messages");
    println!("  Messages contain truncated marker: {}", any_truncated);
    println!("  Session id: {}", id);

    // Second run: resume and invoke the paired reader against the persisted call id.
    let recover_model = ScriptedModel::tool_round(
        vec![call(
            "read-1",
            "read_tool_result",
            json!({"callId": "d1", "maxChars": 2048}),
        )],
        "recovered-full",
    );
    let agent2 = harness.agent(recover_model).build().unwrap();
    let session2 = agent2.resume_session(&id).unwrap();

    let result2 = session2.run("Recover the full data").await.unwrap();
    assert_eq!(result2.text, "recovered-full");
    let recovered = result2.messages.iter().any(|message| {
        serde_json::to_string(message)
            .unwrap_or_default()
            .contains(&"X".repeat(500))
    });
    assert!(recovered, "read_tool_result must return the original body");
    println!("  Resumed session recovered: \"{}\"", result2.text);

    // Fork the session — fork() returns another Session with a clone of the
    // context.  The tests usually only assert id differences; we do that here.
    let fork = session2.fork().await.unwrap();
    assert_ne!(fork.id(), session2.id());
    println!("  Forked session id differs from resumed");

    println!("  OK - Truncation + recovery + fork complete\n");
}

// ===================================================================
// 3 · MANUAL COMPACTION
// ===================================================================
// Note: `on_compact` callbacks on AgentBuilder only fire for automatic
// compaction (Compaction::Automatic).  Manual compaction via
// `session.compact()` returns the CompactReport directly — no callback
// is registered because Compaction::Manual does not wire up LongSession.
// We verify the returned report here instead.

async fn manual_compaction_demo(root: &std::path::Path) {
    println!("=== Manual Compaction ===");

    let harness = Harness::builder().workspace(root).build().unwrap();

    let model = ScriptedModel::new(vec![
        ModelResponse::final_text("Answer 1"),
        ModelResponse::final_text("Answer 2"),
        ModelResponse::final_text("Answer 3"),
    ]);

    // Manual compaction does NOT wire a callback (only Automatic does).
    let agent = harness
        .agent(model)
        .compaction(Compaction::Manual)
        .build()
        .unwrap();

    let session = agent.new_session().persistent().open().unwrap();

    // Build up some history
    session
        .run("Q1 with lots of detail about things.")
        .await
        .unwrap();
    session
        .run("Q2 more detailed information here.")
        .await
        .unwrap();
    session
        .run("Q3 final question asking for summary.")
        .await
        .unwrap();

    let before = session.messages().await;
    println!("  Messages before compact: {}", before.len());
    assert!(before.len() >= 6); // system + 3 turns

    // Manual compact with tight tail budget; report comes back from call.
    let report = session
        .compact(CompactConfig {
            tail_budget_tokens: 10,
        })
        .await
        .unwrap();

    println!(
        "  Report: head={} messages_after={}",
        report.head_messages, report.messages_after
    );
    assert!(report.messages_after < report.messages_before);

    let after = session.messages().await;
    println!("  Messages after compact: {}", after.len());
    assert!(after.len() < before.len());
    assert!(!report.summary.is_empty());
    println!(
        "  Summary length: {} chars, files_read={:?}",
        report.summary.len(),
        report.files_read
    );

    println!("  OK - Manual compaction complete\n");
}

// ===================================================================
// 4 · AUTOMATIC COMPACTION
// ===================================================================

async fn auto_compaction_demo(root: &std::path::Path) {
    println!("=== Automatic Compaction ===");

    let harness = Harness::builder().workspace(root).build().unwrap();

    // Build enough prior turns that before_model crosses the tiny capacity threshold.
    let auto_model = ScriptedModel::new(vec![
        ModelResponse::final_text("first"),
        ModelResponse::final_text("second"),
        ModelResponse::final_text("third"),
    ]);

    let auto_report_log = Arc::new(Mutex::new(Vec::<CompactReport>::new()));
    let auto_log_clone = auto_report_log.clone();

    // Very aggressive compaction: compact at 50% capacity, keep 10% as tail.
    let agent = harness
        .agent(auto_model)
        .context_capacity(24)
        .automatic_compaction(25, 10)
        .on_compact(move |report: CompactReport| {
            // Print first, then store.
            println!(
                "  [auto-compacted] head={} tokens_after={}",
                report.head_messages, report.est_tokens_after
            );
            auto_log_clone.lock().unwrap().push(report);
        })
        .build()
        .unwrap();

    let session = agent.new_session().persistent().open().unwrap();

    session
        .run("First detailed request that grows the conversation history")
        .await
        .unwrap();
    session
        .run("Second detailed request that triggers automatic compaction")
        .await
        .unwrap();
    let result = session.run("Third request after compaction").await.unwrap();
    println!("  Final result: \"{}\"", result.text);

    let reports = auto_report_log.lock().unwrap();
    assert!(
        !reports.is_empty(),
        "automatic compaction callback must fire"
    );
    println!("  Auto-compaction triggered {} time(s)", reports.len());
    println!("  OK - Automatic compaction complete\n");
}

// ===================================================================
// MAIN
// ===================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = temp_dir("recovery-compaction-retry");

    println!(
        "Recovery / Compaction / Retry Example --- workspace: {}\n",
        root.display()
    );

    model_retry_demo(&root).await;
    truncation_recovery_demo(&root).await;
    manual_compaction_demo(&root).await;
    auto_compaction_demo(&root).await;

    std::fs::remove_dir_all(&root)?;
    println!("Temp dir cleaned up.");
    Ok(())
}
