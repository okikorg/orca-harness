//! Fake-model concurrency sweep through the real `SubagentTool` path.
//!
//! Run in release mode. Output is CSV, one row per batch:
//! `cargo run -p orca-harness-tools --release --example bench_subagent_concurrency`

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use orca_harness_core::{
    CancellationToken, Context, Model, ModelError, ModelResponse, Tool, ToolContext, ToolSchema,
};
use orca_harness_tools::{SubagentTool, Workspace};
use serde_json::json;

#[derive(Clone)]
struct FakeModel {
    delay: Duration,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl FakeModel {
    fn new(delay: Duration) -> Self {
        Self {
            delay,
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn reset_peak(&self) {
        self.peak.store(0, Ordering::Relaxed);
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Model for FakeModel {
    async fn generate(
        &self,
        _context: &Context,
        _tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(active, Ordering::Relaxed);
        tokio::time::sleep(self.delay).await;
        self.active.fetch_sub(1, Ordering::Relaxed);
        Ok(ModelResponse::final_text("ok"))
    }
}

fn percentile(sorted: &[u64], percentile: f64) -> u64 {
    let index = ((sorted.len().saturating_sub(1)) as f64 * percentile).round() as usize;
    sorted[index]
}

async fn batch(
    tool: Arc<SubagentTool<FakeModel>>,
    model: &FakeModel,
    n: usize,
    run: usize,
    emit: bool,
) {
    model.reset_peak();
    let started = Instant::now();
    let tasks: Vec<_> = (0..n)
        .map(|i| {
            let tool = tool.clone();
            tokio::spawn(async move {
                let call_started = Instant::now();
                let context = ToolContext {
                    call_id: format!("{run}-{i}"),
                    tool_name: "subagent".into(),
                    cancellation: CancellationToken::new(),
                    deadline: None,
                };
                let result = tool
                    .call(json!({"task": format!("fake-{run}-{i}")}), &context)
                    .await;
                let ok = result
                    .as_ref()
                    .is_ok_and(|output| output["answer"].as_str() == Some("ok"));
                (ok, call_started.elapsed().as_micros() as u64)
            })
        })
        .collect();
    let mut results = Vec::with_capacity(n);
    for task in tasks {
        results.push(task.await);
    }
    let wall_us = started.elapsed().as_micros() as u64;
    let mut latencies = Vec::with_capacity(n);
    let mut failures = 0;
    for result in results {
        match result {
            Ok((true, latency)) => latencies.push(latency),
            Ok((false, latency)) => {
                failures += 1;
                latencies.push(latency);
            }
            Err(_) => failures += 1,
        }
    }
    latencies.sort_unstable();
    let p50 = percentile(&latencies, 0.50);
    let p95 = percentile(&latencies, 0.95);
    let p99 = percentile(&latencies, 0.99);
    let throughput = n as f64 * 1_000_000.0 / wall_us.max(1) as f64;
    if emit {
        println!(
            "{n},{run},{wall_us},{throughput:.3},{p50},{p95},{p99},{failures},{}",
            model.peak()
        );
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let levels: Vec<usize> = std::env::var("LEVELS")
        .unwrap_or_else(|_| "1,2,4,8,16,32,64,128,256,512,1024,2048,4096,8192,16384".into())
        .split(',')
        .map(|value| {
            value
                .parse()
                .expect("LEVELS must be comma-separated integers")
        })
        .collect();
    let delay_ms = std::env::var("DELAY_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let model = FakeModel::new(Duration::from_millis(delay_ms));
    let workspace = Workspace::new(std::env::temp_dir().join("orca-subagent-concurrency-bench"));
    std::fs::create_dir_all(workspace.root()).expect("create benchmark workspace");
    let tool = Arc::new(SubagentTool::new(model.clone(), &workspace));

    eprintln!("warming up");
    batch(tool.clone(), &model, 64, usize::MAX, false).await;
    println!("fanout,run,wall_us,throughput_per_s,p50_us,p95_us,p99_us,failures,peak_active");
    let repetitions_override = std::env::var("REPETITIONS")
        .ok()
        .and_then(|value| value.parse().ok());
    for n in levels {
        let repetitions = repetitions_override.unwrap_or(if n <= 256 {
            20
        } else if n <= 2048 {
            10
        } else {
            5
        });
        for run in 0..repetitions {
            batch(tool.clone(), &model, n, run, true).await;
        }
    }
}
