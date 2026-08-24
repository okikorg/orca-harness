//! Compaction prototype demo on real data: runs a live agent session
//! (local OpenAI-compatible endpoint, real tools, real transcript), then
//! compacts the context deterministically and dumps before/after as JSON.
//!
//! Run with:
//!   cargo run -p orca-harness-extensions --example compact_demo -- \
//!     [--model qwen3.5:9b] [--base-url http://localhost:11434/v1] \
//!     [--workspace DIR] [--out report.json] [--tail-budget N] \
//!     [--replay prior_report.json]
//!
//! --replay compacts a previous report's "before" transcript directly
//! (compaction is deterministic, so no live session is needed).

use std::sync::Arc;

use serde_json::json;

use orca_harness_core::{Agent, CancellationToken, Context, Limits, Message};
use orca_harness_extensions::{
    compact, CompactConfig, ReadToolResultTool, Truncation, TruncationStore, UsageMeter,
};
use orca_harness_model_providers::openai::OpenAiModel;
use orca_harness_tools::{core_tools, Workspace};

const PROMPTS: &[&str] = &[
    "List the top-level files in this workspace and read README.md. \
     In two sentences, what is this project?",
    "Read crates/harness-core/src/context.rs and \
     crates/harness-core/src/agent_loop.rs. Briefly explain how messages \
     flow through the agent loop.",
    "Grep the crates directory for 'truncation' and read the main file \
     implementing it. What does the truncation extension do?",
    "Read crates/cli/src/tui.rs and describe its main state structures \
     in a few sentences.",
    "Read crates/cli/src/view.rs and crates/harness-core/src/dispatcher.rs. \
     How does tool dispatch relate to what the view renders?",
    "Grep the crates directory for 'WorkerCmd' and read \
     crates/cli/src/msg.rs. List the worker commands.",
    "Read crates/cli/src/main.rs. How does the worker task own the \
     conversation across runs?",
    "Read crates/cli/src/approval.rs and crates/cli/src/headless.rs. \
     How do tool approvals differ between the TUI and headless mode?",
];

fn arg(name: &str, default: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| default.to_string())
}

fn snapshot(messages: &[Message]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|m| {
            let bytes = serde_json::to_string(m).map(|s| s.len()).unwrap_or(0);
            json!({ "bytes": bytes, "message": m })
        })
        .collect()
}

/// Snapshot, compact, snapshot, write the report.
fn finish(
    mut context: Context,
    mut meta: serde_json::Value,
    store: &TruncationStore,
    tail_budget: usize,
    out: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let before = snapshot(context.messages());
    let config = CompactConfig {
        tail_budget_tokens: tail_budget,
    };
    let started = std::time::Instant::now();
    let report = compact(&mut context, store, &config)?;
    let compact_us = started.elapsed().as_micros();
    let after = snapshot(context.messages());

    meta["tailBudgetTokens"] = json!(tail_budget);
    meta["compactDurationUs"] = json!(compact_us as u64);
    let payload = json!({
        "meta": meta,
        "report": report,
        "before": before,
        "after": after,
    });
    std::fs::write(out, serde_json::to_string_pretty(&payload)?)?;
    eprintln!(
        "compacted: {} -> {} bytes (est {} -> {} tokens, {:.2}%), report at {out}",
        report.bytes_before,
        report.bytes_after,
        report.est_tokens_before,
        report.est_tokens_after,
        100.0 * report.est_tokens_after as f64 / report.est_tokens_before.max(1) as f64,
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model_id = arg("--model", "qwen3.5:9b");
    let base_url = arg("--base-url", "http://localhost:11434/v1");
    let workspace = arg("--workspace", ".");
    let out = arg("--out", "compact_report.json");
    let tail_budget: usize = arg("--tail-budget", "0").parse()?;
    let replay = arg("--replay", "");

    let store = TruncationStore::default();

    // Replay mode: compact a previous report's "before" transcript.
    if !replay.is_empty() {
        let prior: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&replay)?)?;
        let mut context = Context::new();
        for entry in prior["before"].as_array().expect("before array") {
            let message: Message = serde_json::from_value(entry["message"].clone())?;
            context.push(message);
        }
        return finish(context, prior["meta"].clone(), &store, tail_budget, &out);
    }

    let ws = Workspace::new(&workspace);
    let model = Arc::new(OpenAiModel::new(&model_id).base_url(&base_url));
    let (meter, usage) = UsageMeter::new();

    let mut agent = Agent::new(model.clone())
        .limits(Limits::default())
        .extension(Truncation::new(16_000).store(store.clone()))
        .extension(meter)
        .tool_arc(Arc::new(ReadToolResultTool::new(store.clone())));
    for tool in core_tools(&ws) {
        agent = agent.tool_arc(tool);
    }

    let mut context = Context::new();
    context.push_system(format!(
        "You are Orca, a coding agent operating in the workspace at {} . \
         You act through tools: shell, read_file, write_file, edit_file, \
         list_dir, grep, glob, read_tool_result. File paths are \
         workspace-relative. Investigate with tools instead of guessing. \
         Keep responses brief and concrete.",
        ws.root().display()
    ));

    for prompt in PROMPTS {
        eprintln!(">>> {prompt}");
        context.push_user(*prompt);
        let answer = agent
            .run_context(&mut context, CancellationToken::new())
            .await?;
        eprintln!("<<< {answer}\n");
    }

    let total = usage.total();
    let meta = json!({
        "model": model_id,
        "baseUrl": base_url,
        "workspace": ws.root().display().to_string(),
        "prompts": PROMPTS,
        "runUsage": {
            "inputTokens": total.input_tokens,
            "outputTokens": total.output_tokens,
            "meteredSteps": usage.metered_steps(),
        },
    });
    finish(context, meta, &store, tail_budget, &out)
}
