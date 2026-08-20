//! End-to-end run against a fake LLM, showing concurrent tool fan-out.
//!
//! Run with: cargo run --example fake_llm

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::json;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    Agent, Extension, FnTool, Limits, ModelResponse, Subscriptions, ToolResult,
};

struct PrintTelemetry;

#[async_trait::async_trait]
impl Extension for PrintTelemetry {
    fn name(&self) -> &str {
        "print-telemetry"
    }
    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().tool_result()
    }
    async fn tool_result(&self, result: &ToolResult) {
        println!("  observed {} → {}", result.call_id, result.output);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The fake LLM asks for four concurrent "fetch" calls, then answers.
    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("Fetching four sources in parallel.".into()),
            calls: (0..4)
                .map(|i| call(&format!("call_{i}"), "fetch", json!({"source": i})))
                .collect(),
            usage: None,
        },
        ModelResponse::final_text("All four sources fetched concurrently."),
    ]);

    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let fetch = {
        let in_flight = in_flight.clone();
        let peak = peak.clone();
        FnTool::new(
            "fetch",
            "Fetch a data source",
            json!({"type": "object", "properties": {"source": {"type": "integer"}}}),
            move |input, _ctx| {
                let in_flight = in_flight.clone();
                let peak = peak.clone();
                async move {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(json!({"source": input["source"], "bytes": 1024}))
                }
            },
        )
    };

    let agent = Agent::new(model)
        .system_prompt("You are a concurrent fetcher.")
        .tool(fetch)
        .extension(PrintTelemetry)
        .limits(Limits {
            max_parallel_tools: 8,
            ..Limits::default()
        });

    let started = std::time::Instant::now();
    let answer = agent.run("Fetch sources 0..4").await?;
    println!("answer: {answer}");
    println!(
        "peak concurrency: {} — wall clock {:?} (4 × 100ms of tool work)",
        peak.load(Ordering::SeqCst),
        started.elapsed()
    );
    Ok(())
}
