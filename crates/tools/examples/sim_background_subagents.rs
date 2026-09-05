//! Realistic background-subagent capacity simulation.
//!
//! Every worker does real work through the real path: `SubagentTool::call`
//! with `background: true`, the session manager's running-slot gate, a
//! fresh inner `Agent` with the real core tools against a generated
//! documentation tree, and the interactive host's per-spawn extension
//! shape (an event stream feeding a consumer that stands in for the TUI,
//! output truncation, mutation preflight, the model retry wrapper). Only
//! the model is simulated, and it behaves like a provider: time to first
//! token, streamed reasoning, tool-input and text deltas at a token rate,
//! token accounting, and a four-step plan whose calls depend on the
//! previous tool results — list the area, grep it and read files, run a
//! shell grep, then answer with counts that are checked against ground
//! truth. A wrong answer is a failure, not a completion.
//!
//! ```text
//! cargo run -p orca-harness-tools --release --example sim_background_subagents
//! ```
//!
//! Environment:
//! - `SIM_LEVELS`: running limits to sweep through the background path
//!   (default `1,2,4,8,16,32,64`).
//! - `SIM_RAW_LEVELS`: foreground fan-out levels, same
//!   per-worker work without the manager (default `128,256,512`; empty to
//!   skip).
//! - `SIM_SCALE`: multiplier on model timing (default `1.0`; `0.2` for a
//!   quick pass).
//! - `SIM_PROVIDER_CAP`: simulate a provider that answers `429` above this
//!   many concurrent streams (default off). Rejections go through the same
//!   `RetryModel` policy the CLI installs.
//! - `SIM_MODEL_CONCURRENCY`: shared model stream limit (default off to measure
//!   harness capacity); set to the provider cap to verify admission.
//! - `SIM_REPEAT`: repetitions per level (default: 4 at limit ≤ 2, 2 at
//!   ≤ 8, else 1).
//!
//! Output: one CSV row per level on stdout; a summary against the
//! single-worker baseline on stderr.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::{
    CancellationToken, Context, DeltaSink, Extension, Message, Model, ModelDelta, ModelError,
    ModelResponse, Next, Subscriptions, Tool, ToolCall, ToolContext, ToolError, ToolSchema, Usage,
};
use orca_harness_extensions::{EventStream, HarnessEvent, ModelGate, RetryModel, Truncation};
use orca_harness_tools::{
    BackgroundStats, MutationPreflight, SubagentManager, SubagentNotification, SubagentSpawn,
    SubagentTool, Workspace,
};

mod sim_background_support;
use sim_background_support::*;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let levels = env_list("SIM_LEVELS", "1,2,4,8,16,32,64");
    let raw_levels = env_list("SIM_RAW_LEVELS", "128,256,512");
    let scale = env_f64("SIM_SCALE", 1.0);
    let provider_cap = std::env::var("SIM_PROVIDER_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|cap| *cap > 0);
    let repeat_override = std::env::var("SIM_REPEAT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());

    let root: PathBuf =
        std::env::temp_dir().join(format!("orca-sim-background-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("workspace root");
    eprintln!("building fixture in {}", root.display());
    let fixture = build_fixture(&root);
    let max_lines = fixture
        .truths
        .values()
        .map(|truth| truth.lines)
        .max()
        .unwrap_or(0);
    eprintln!(
        "fixture: {AREAS} areas x {FILES_PER_AREA} files, {} topics, densest (area, topic) has {max_lines} matching lines",
        TOPICS.len()
    );
    eprintln!(
        "model: scale {scale}, {TOKENS_PER_SECOND} tok/s, ttft 450-1350ms x scale, provider cap {}",
        provider_cap.map_or("off".to_string(), |cap| cap.to_string())
    );

    let model = Arc::new(SimModel::new(scale, provider_cap));
    // The CLI wraps every endpoint in this retry policy; workers inherit it.
    let mut retry = RetryModel::new(model.clone(), None)
        .retry_delay(orca_harness_model_providers::http_error::retry_delay);
    let model_concurrency = std::env::var("SIM_MODEL_CONCURRENCY").ok();
    if let Some(value) = &model_concurrency {
        let limit: usize = value
            .parse()
            .expect("SIM_MODEL_CONCURRENCY must be positive");
        assert!(limit > 0);
        eprintln!("shared model concurrency: {limit}");
        retry = retry.gate(ModelGate::new(limit));
    }
    let shared: SharedModel = Arc::new(retry);
    let harness = Harness {
        model,
        shared,
        workspace: Workspace::new(&root),
        fixture,
        stats: BackgroundStats::new(),
        timing: ToolTiming::default(),
        events: EventConsumer::start(),
    };

    eprintln!("warming up");
    let _ = harness.background_level(4, 4, 0).await;

    println!(
        "mode,limit,spawned,reps,wall_s,agents_per_s,lat_p50_s,lat_p95_s,lat_max_s,model_calls,\
         model_lag_p50_ms,model_lag_p95_ms,model_lag_max_ms,peak_model_inflight,rejected_429,\
         tool_calls,tool_p50_ms,tool_p95_ms,tool_max_ms,shell_p95_ms,tool_errors,events,\
         event_lag_p95_ms,event_lag_max_ms,failures,wrong_answers,rss_mb"
    );
    let mut rows = Vec::new();
    let mut offset = 1_000;
    for limit in levels {
        let reps = repeat_override.unwrap_or(if limit <= 2 {
            4
        } else if limit <= 8 {
            2
        } else {
            1
        });
        let mut metrics = LevelMetrics::default();
        for _ in 0..reps {
            eprintln!("background limit {limit}: {limit} workers");
            metrics.absorb(harness.background_level(limit as u32, limit, offset).await);
            offset += limit;
        }
        let row = Row {
            mode: "background",
            limit,
            spawned: limit,
            reps,
            metrics,
        };
        print_row(&row);
        rows.push(row);
    }
    for spawned in raw_levels {
        eprintln!("raw fan-out: {spawned} workers");
        let metrics = harness.raw_level(spawned, offset).await;
        offset += spawned;
        let row = Row {
            mode: "raw",
            limit: spawned,
            spawned,
            reps: 1,
            metrics,
        };
        print_row(&row);
        rows.push(row);
    }
    print_summary(&rows, model_concurrency.is_some());
    eprintln!(
        "in-flight agents after the sweep: {} (expect 0)",
        harness.stats.agents()
    );
    let _ = std::fs::remove_dir_all(&root);
}
