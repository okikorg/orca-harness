//! Full agent-loop benchmark.
//!
//! `dispatch.rs` measures the Dispatcher in isolation — pure tool
//! dispatch overhead. It does not exercise the loop, context assembly,
//! model boundary, or extension hooks, which is exactly the layer the
//! "best AI agent harness" comparisons put under the microscope.
//!
//! This bench drives the *whole* `Agent` end-to-end with a
//! [`ReproducingModel`]: a fake model that regenerates the *same* scripted
//! response on every `generate` call (unlike
//! [`ScriptedModel`](orca_harness_core::testing::ScriptedModel), which
//! drains a one-shot queue), so the bench measures steady-state harness
//! round-trips — model invocation, context appends (assistant tool calls +
//! tool results), concurrent execution, and extension hooks — as a team
//! would actually run it, not a one-shot drain.

use std::sync::atomic::AtomicUsize;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use serde_json::{json, Value};

use orca_harness_core::testing::call;
use orca_harness_core::{
    Agent, Context, Extension, FnTool, Limits, Model, ModelDelta, ModelError, ModelResponse,
    Subscriptions, ToolCall, ToolContext, ToolResult, Usage,
};

/// A fake model that replays one fixed script of responses on every
/// `generate` call. Unlike
/// [`ScriptedModel`](orca_harness_core::testing::ScriptedModel), which
/// drains a one-shot queue, it regenerates the script so the bench can
/// measure *steady-state* harness round-trips, not a one-shot drain.
pub struct ReproducingModel {
    script: Vec<ModelResponse>,
    calls: AtomicUsize,
}

impl ReproducingModel {
    fn new(script: Vec<ModelResponse>) -> Self {
        Self {
            script,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Model for ReproducingModel {
    async fn generate(
        &self,
        _context: &Context,
        _tools: &[orca_harness_core::ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        // Return the *next* response in the script each call, looping, so
        // an iteration alternates tool-call batches and final answers and
        // a fresh run sees the same script as every other.
        let step = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.script[step % self.script.len()].clone())
    }
}

/// A tool with real (tiny) work and a per-call latency mask, so the
/// harness overhead is measured against a latency curve rather than
/// against zero. `latency` lets us show what fan-out buys us: `n`
/// serialized `latency`-length calls collapse to ~`latency` total.
fn fetch(latency: std::time::Duration) -> FnTool {
    FnTool::new(
        "fetch",
        "Fetch a data source concurrently",
        json!({"type": "object", "properties": {"source": {"type": "integer"}}}),
        move |_input, _ctx| async move {
            tokio::time::sleep(latency).await;
            Ok(Value::Null)
        },
    )
}

/// A compact extension that subscribes to every hot-path hook and calls
/// through faithfully, standing in for the realistic set (event stream,
/// telemetry, usage metering) a team registers. Doing nothing but
/// round-tripping each hook measures the extension boundary itself.
struct BusyExtension;

#[async_trait::async_trait]
impl Extension for BusyExtension {
    fn name(&self) -> &str {
        "busy"
    }
    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none()
            .before_model()
            .model_delta()
            .after_model()
            .before_tool()
            .around_tool()
            .after_tool()
            .tool_result()
    }

    async fn before_model(
        &self,
        _context: &mut Context,
    ) -> Result<(), orca_harness_core::ExtensionError> {
        let _ = std::hint::black_box(0);
        Ok(())
    }
    async fn on_model_delta(&self, _delta: &ModelDelta) {
        let _ = std::hint::black_box(0);
    }
    async fn after_model(
        &self,
        _context: &mut Context,
        _response: &ModelResponse,
    ) -> Result<(), orca_harness_core::ExtensionError> {
        let _ = std::hint::black_box(0);
        Ok(())
    }
    async fn before_tool(
        &self,
        _call: &ToolCall,
    ) -> Result<orca_harness_core::ToolDecision, orca_harness_core::ExtensionError> {
        let _ = std::hint::black_box(0);
        Ok(orca_harness_core::ToolDecision::Continue)
    }
    async fn around_tool<'a>(
        &self,
        _call: &ToolCall,
        input: Value,
        _ctx: &ToolContext,
        next: orca_harness_core::Next<'a>,
    ) -> Result<Value, orca_harness_core::ToolError> {
        // Call through without alteration; the cost of just being present
        // on the wrapper chain.
        next.run(input).await
    }
    async fn after_tool(
        &self,
        _call: &ToolCall,
        result: ToolResult,
    ) -> Result<ToolResult, orca_harness_core::ExtensionError> {
        let _ = std::hint::black_box(0);
        Ok(result)
    }
    async fn tool_result(&self, _result: &ToolResult) {
        let _ = std::hint::black_box(0);
    }
}

/// Script: one round of `n` parallel tool calls, then a final answer.
/// Because the model loops over the script, the final-answer response
/// terminates the run each iteration.
fn script(n: usize) -> Vec<ModelResponse> {
    vec![
        ModelResponse::ToolCalls {
            content: Some("fetching sources".into()),
            calls: (0..n)
                .map(|i| call(&format!("call_{i}"), "fetch", json!({"source": i})))
                .collect(),
            usage: Some(Usage {
                input_tokens: 512,
                output_tokens: 32,
                cache_read_tokens: 128,
                ..Default::default()
            }),
        },
        ModelResponse::final_text("done"),
    ]
}

fn build_agent(latency: std::time::Duration, n: usize) -> (Agent<ReproducingModel>, String) {
    (
        Agent::new(ReproducingModel::new(script(n)))
            .system_prompt("You are a concurrent fetcher.")
            .tool(fetch(latency))
            .extension(BusyExtension)
            .limits(Limits {
                max_parallel_tools: 64,
                ..Limits::default()
            }),
        format!("fetch {n} sources"),
    )
}

fn bench_full_loop(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    // Full agent round-trip across a single parallel tool batch at
    // increasing fan-out, with zero tool latency, so we measure pure
    // harness overhead on the model→tools→model path.
    let mut group = c.benchmark_group("harness_loop");
    for n in [1usize, 4, 16, 64] {
        let (agent, prompt) = build_agent(std::time::Duration::ZERO, n);
        group.bench_with_input(BenchmarkId::new("round_trip_n_calls", n), &n, |b, &_n| {
            b.to_async(&rt).iter(|| agent.run(&prompt));
        });
    }
    group.finish();

    // Fan-out with real per-call latency: show that `latency`-length work
    // spread across `n` parallel calls collapses to ~`latency` + harness
    // overhead — the headline property of the kernel.
    let latency = std::time::Duration::from_micros(200);
    let mut group = c.benchmark_group("harness_loop_fanout");
    for n in [1usize, 8, 64] {
        let (agent, prompt) = build_agent(latency, n);
        group.bench_with_input(BenchmarkId::new("parallel_200us_calls", n), &n, |b, &_n| {
            b.to_async(&rt).iter(|| agent.run(&prompt))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_full_loop);
criterion_main!(benches);
