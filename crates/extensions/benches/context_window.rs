//! What it costs to keep a long session inside its context window.
//!
//! Three costs, on three different clocks:
//!
//! - **Truncation** runs in `after_tool`, once per tool result, on the
//!   agent's critical path — between a tool returning and the model seeing
//!   it. It walks the whole result twice (a `needs_truncation` probe, then
//!   a rewrite) and, with a store attached, serializes the original and
//!   inserts it under a byte budget that may evict. A grep or a file read
//!   is the common case and can be megabytes.
//! - **`read_tool_result`** is the model paging back through something
//!   that was truncated: one store lookup and a character slice, on the
//!   critical path between the model asking and continuing.
//! - **`compact`** runs once, when the window fills, and blocks the turn
//!   it fires on. It copies the transcript, elides head tool results into
//!   the store, derives a mechanical summary, and rebuilds the context —
//!   every step linear in the transcript, which is at its largest exactly
//!   when this runs.
//!
//! `memory.rs` measures the other half of the long-session story: what
//! recall costs against a populated store. This file measures what
//! *forgetting* costs.
//!
//! Sizes are chosen from where each path actually hurts. Tool outputs run
//! to megabytes because that is what a wide grep returns; transcripts run
//! to a thousand messages because a compaction that fires earlier had less
//! to copy.

use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use serde_json::{json, Value};

use orca_harness_core::{
    CancellationToken, Context, Extension, Tool, ToolCall, ToolContext, ToolResult,
};
use orca_harness_extensions::{
    compact, CompactConfig, ReadToolResultTool, Truncation, TruncationStore,
};

/// The default the CLI runs with; truncating at a different limit would
/// measure a configuration nobody ships.
const MAX_CHARS: usize = 8 * 1024;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "grep".into(),
        arguments: json!({"pattern": "fn "}),
    }
}

/// One flat string of `bytes` — a file read. The cheapest shape to walk
/// and the most common one to truncate.
fn flat_output(bytes: usize) -> Value {
    json!({"content": "x".repeat(bytes)})
}

/// `lines` match objects — a grep. Same total size as `flat_output` at
/// the same byte count, but thousands of small strings, so the recursive
/// walk pays per node rather than per byte. Only the file bodies are long
/// enough to truncate, so the walk cannot short-circuit early.
fn structured_output(lines: usize) -> Value {
    json!({
        "matches": (0..lines)
            .map(|i| json!({
                "path": format!("crates/some-crate/src/module_{}.rs", i % 64),
                "line": i,
                "text": "    pub fn handle(&self, input: Value) -> Result<Value, Error> {",
            }))
            .collect::<Vec<_>>(),
        "body": "x".repeat(MAX_CHARS * 2),
    })
}

/// A transcript of `turns` user/assistant/tool rounds, shaped like real
/// work: every turn calls a tool and gets a sizeable result back, which is
/// what makes a window fill in the first place.
fn transcript(turns: usize) -> Context {
    let mut context = Context::new();
    context.push_system("You are a coding agent operating in a git repository.");
    for i in 0..turns {
        context.push_user(format!("Turn {i}: find and fix the failing assertion."));
        context.push_assistant_tool_calls(
            Some(format!("Reading the module for turn {i}.")),
            vec![ToolCall {
                id: format!("call-{i}"),
                name: if i % 2 == 0 { "read_file" } else { "edit_file" }.into(),
                arguments: json!({"path": format!("crates/core/src/mod_{i}.rs")}),
            }],
        );
        context.append_tool_results(vec![ToolResult {
            call_id: format!("call-{i}"),
            tool_name: if i % 2 == 0 { "read_file" } else { "edit_file" }.into(),
            output: json!({"content": "y".repeat(2_048)}),
            is_error: false,
        }]);
    }
    context
}

fn tool_context(call_id: &str) -> ToolContext {
    ToolContext {
        call_id: call_id.into(),
        tool_name: "read_tool_result".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

/// The `after_tool` hook, with and without a store. The difference between
/// the two is what durable recovery costs: a serialize plus a budgeted
/// insert on top of the walk.
fn truncation(c: &mut Criterion) {
    let rt = runtime();
    let plain = Truncation::new(MAX_CHARS);
    let mut group = c.benchmark_group("context/truncate");

    for bytes in [64 * 1024usize, 512 * 1024, 4 * 1024 * 1024] {
        let output = flat_output(bytes);
        group.throughput(criterion::Throughput::Bytes(bytes as u64));
        group.bench_with_input(BenchmarkId::new("flat", bytes), &output, |b, output| {
            b.to_async(&rt).iter(|| {
                let result = ToolResult::ok(&call("c0"), output.clone());
                async { plain.after_tool(&call("c0"), result).await.unwrap() }
            })
        });
    }

    for lines in [1_000usize, 10_000, 50_000] {
        let output = structured_output(lines);
        group.throughput(criterion::Throughput::Elements(lines as u64));
        group.bench_with_input(
            BenchmarkId::new("structured", lines),
            &output,
            |b, output| {
                b.to_async(&rt).iter(|| {
                    let result = ToolResult::ok(&call("c0"), output.clone());
                    async { plain.after_tool(&call("c0"), result).await.unwrap() }
                })
            },
        );
    }

    // With a store, sized so the run evicts: the eviction path is the one
    // a long session is always on, and it is not the fast path.
    let output = flat_output(512 * 1024);
    for budget in [64 * 1024 * 1024usize, 1024 * 1024] {
        let name = if budget > 8 * 1024 * 1024 {
            "stored_roomy"
        } else {
            "stored_evicting"
        };
        group.throughput(criterion::Throughput::Bytes(512 * 1024));
        group.bench_function(BenchmarkId::new(name, budget), |b| {
            let ext = Truncation::new(MAX_CHARS).store(TruncationStore::new(budget));
            let mut seq = 0u64;
            b.to_async(&rt).iter(|| {
                seq += 1;
                let call = call(&format!("c{seq}"));
                let result = ToolResult::ok(&call, output.clone());
                let ext = &ext;
                async move { ext.after_tool(&call, result).await.unwrap() }
            })
        });
    }
    group.finish();
}

/// Paging a stored original back in: the model's `read_tool_result` call.
/// Slice size is swept from a small page up to the tool's own 64 KiB cap.
/// The cost turns out to be flat in slice size — reaching a character
/// offset inside the stored original dominates the copy out of it — so
/// there is no throughput on this group: dividing one flat cost by a
/// varying slice would manufacture a scaling curve that does not exist.
fn read_back(c: &mut Criterion) {
    let rt = runtime();
    let store = TruncationStore::new(64 * 1024 * 1024);
    let ext = Truncation::new(MAX_CHARS).store(store.clone());
    let seeded = call("stored");
    rt.block_on(async {
        let result = ToolResult::ok(&seeded, flat_output(4 * 1024 * 1024));
        ext.after_tool(&seeded, result).await.unwrap();
    });
    let tool = Arc::new(ReadToolResultTool::new(store));

    let mut group = c.benchmark_group("context/read_back");
    for chars in [4 * 1024usize, 32 * 1024, 64 * 1024] {
        group.bench_with_input(BenchmarkId::new("slice", chars), &chars, |b, chars| {
            let input = json!({"callId": "stored", "chars": chars, "offset": 1_000});
            b.to_async(&rt).iter(|| {
                let tool = tool.clone();
                let input = input.clone();
                async move {
                    black_box(tool.call(input, &tool_context("read")).await.unwrap());
                }
            })
        });
    }
    group.finish();
}

/// Compaction, at the transcript sizes it realistically fires on. The
/// context is rebuilt from a clone each iteration because compaction is
/// destructive and a second pass over a compacted context measures
/// nothing.
fn compaction(c: &mut Criterion) {
    let config = CompactConfig::default();
    let mut group = c.benchmark_group("context/compact");
    for turns in [50usize, 250, 1_000] {
        let context = transcript(turns);
        // Three messages per turn plus the system prompt.
        group.throughput(criterion::Throughput::Elements(turns as u64 * 3 + 1));
        group.bench_with_input(BenchmarkId::new("turns", turns), &context, |b, context| {
            b.iter_batched(
                || {
                    let mut fresh = Context::new();
                    for message in context.messages() {
                        fresh.push(message.clone());
                    }
                    (fresh, TruncationStore::new(64 * 1024 * 1024))
                },
                |(mut context, store)| {
                    let report = compact(&mut context, &store, &config).unwrap();
                    // Returned, not dropped here: freeing the rebuilt
                    // context and a store full of elided results is
                    // teardown, and belongs outside the timer.
                    (context, store, report)
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

criterion_group!(benches, truncation, read_back, compaction);
criterion_main!(benches);
