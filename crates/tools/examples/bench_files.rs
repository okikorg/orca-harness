//! File-tool concurrency benchmark against a real `.bench` folder.
//!
//! Seeds N files, then drives the real Dispatcher through three phases —
//! read_file, edit_file, delete_file — each as one N-call concurrent
//! batch, and compares each against a serialized run (max_parallel = 1)
//! of the same work so the speedup number is grounded.
//!
//! Run with:  cargo run -p orca-harness-tools --release --example bench_files
//! Optional arg: <file-count> (default 100)

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use orca_harness_core::testing::call;
use orca_harness_core::{
    CancellationToken, Dispatcher, ExtensionRegistry, ToolCall, ToolRegistry, ToolResult,
};
use orca_harness_tools::{DeleteFileTool, EditFileTool, ReadFileTool, Workspace};

const CONTENT_LINES: usize = 64;

fn seed(dir: &Path, n: usize, marker: &str) {
    for i in 0..n {
        let mut body = String::new();
        for line in 0..CONTENT_LINES {
            body.push_str(&format!("file {i} line {line} payload payload payload\n"));
        }
        body.push_str(&format!("{marker}-{i}\n"));
        std::fs::write(dir.join(format!("f{i}.txt")), body).unwrap();
    }
}

async fn run_batch(
    tools: &ToolRegistry,
    batch: Vec<ToolCall>,
    max_parallel: usize,
) -> (Duration, Vec<ToolResult>) {
    let started = Instant::now();
    let results = Dispatcher::new()
        .execute(
            batch,
            tools,
            &ExtensionRegistry::new(),
            &CancellationToken::new(),
            None,
            max_parallel,
        )
        .await
        .unwrap();
    (started.elapsed(), results)
}

fn report(phase: &str, n: usize, serial: Duration, concurrent: Duration, results: &[ToolResult]) {
    let errors = results.iter().filter(|r| r.is_error).count();
    let per_call = concurrent.as_secs_f64() * 1e6 / n as f64;
    let throughput = n as f64 / concurrent.as_secs_f64();
    let speedup = serial.as_secs_f64() / concurrent.as_secs_f64();
    println!("── {phase}  (n={n}) ──");
    println!(
        "   serialized (max_parallel=1)   {:>9.2} ms",
        serial.as_secs_f64() * 1e3
    );
    println!(
        "   concurrent (max_parallel={n})  {:>9.2} ms   ({per_call:.1} µs/call)",
        concurrent.as_secs_f64() * 1e3
    );
    println!(
        "   speedup {speedup:>5.1}×   throughput {throughput:>9.0} calls/sec   errors {errors}"
    );
    if errors > 0 {
        for r in results.iter().filter(|r| r.is_error).take(3) {
            println!("   ! {}: {}", r.call_id, r.output);
        }
    }
    println!();
}

/// The edit tool's exact work — read, replacen, write — via plain
/// tokio::spawn with no dispatcher, tools, or workspace resolution.
async fn raw_edit_pass(dir: &Path, n: usize, from: &str, to: &str) -> Duration {
    let started = Instant::now();
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let path = dir.join(format!("f{i}.txt"));
        let old = format!("{from}-{i}");
        let new = format!("{to}-{i}");
        handles.push(tokio::spawn(async move {
            let content = tokio::fs::read_to_string(&path).await.unwrap();
            let updated = content.replacen(&old, &new, 1);
            tokio::fs::write(&path, updated).await.unwrap();
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
    started.elapsed()
}

/// The same edit work, but the whole read+replace+write runs inside ONE
/// spawn_blocking closure (std::fs), instead of two separate blocking
/// hops with an async bounce between them.
async fn raw_edit_single_hop_pass(dir: &Path, n: usize, from: &str, to: &str) -> Duration {
    let started = Instant::now();
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let path = dir.join(format!("f{i}.txt"));
        let old = format!("{from}-{i}");
        let new = format!("{to}-{i}");
        handles.push(tokio::task::spawn_blocking(move || {
            let content = std::fs::read_to_string(&path).unwrap();
            let updated = content.replacen(&old, &new, 1);
            std::fs::write(&path, updated).unwrap();
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
    started.elapsed()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100);

    let dir = std::env::current_dir().unwrap().join(".bench");
    std::fs::create_dir_all(&dir).unwrap();
    let ws = Workspace::new(dir.clone());

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ReadFileTool::new(ws.clone())));
    tools.register(Arc::new(EditFileTool::new(ws.clone())));
    tools.register(Arc::new(DeleteFileTool::new(ws.clone())));

    println!(
        "\nFile-tool benchmark — {} files in {}, {} worker threads\n",
        n,
        dir.display(),
        std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(0),
    );

    let reads = |n: usize| -> Vec<ToolCall> {
        (0..n)
            .map(|i| {
                call(
                    &format!("r{i}"),
                    "read_file",
                    json!({"path": format!("f{i}.txt")}),
                )
            })
            .collect()
    };
    let edits = |n: usize, from: &str, to: &str| -> Vec<ToolCall> {
        (0..n)
            .map(|i| {
                call(
                    &format!("e{i}"),
                    "edit_file",
                    json!({
                        "path": format!("f{i}.txt"),
                        "old": format!("{from}-{i}"),
                        "new": format!("{to}-{i}"),
                    }),
                )
            })
            .collect()
    };
    let deletes = |n: usize| -> Vec<ToolCall> {
        (0..n)
            .map(|i| {
                call(
                    &format!("d{i}"),
                    "delete_file",
                    json!({"path": format!("f{i}.txt")}),
                )
            })
            .collect()
    };

    // Phase 1: reads. Same files both passes; reads don't mutate.
    seed(&dir, n, "MARK-A");
    let (serial, _) = run_batch(&tools, reads(n), 1).await;
    let (concurrent, results) = run_batch(&tools, reads(n), n).await;
    report("read_file", n, serial, concurrent, &results);

    // Phase 2: edits. The serialized pass rewrites A→B, the concurrent
    // pass B→C, so both do identical real work on warm files. A raw
    // tokio pass (C→D, no harness) isolates dispatch overhead from
    // filesystem behavior.
    let (serial, _) = run_batch(&tools, edits(n, "MARK-A", "MARK-B"), 1).await;
    let (concurrent, results) = run_batch(&tools, edits(n, "MARK-B", "MARK-C"), n).await;
    report("edit_file", n, serial, concurrent, &results);
    let (waves, _) = run_batch(&tools, edits(n, "MARK-C", "MARK-D"), 8).await;
    println!(
        "   waves of 8 (max_parallel=8, the default)   {:>9.2} ms",
        waves.as_secs_f64() * 1e3
    );
    let raw = raw_edit_pass(&dir, n, "MARK-D", "MARK-E").await;
    println!(
        "   raw tokio baseline (no harness, same read+replace+write) {:>9.2} ms",
        raw.as_secs_f64() * 1e3
    );
    let single = raw_edit_single_hop_pass(&dir, n, "MARK-E", "MARK-F").await;
    println!(
        "   raw single-hop baseline (one spawn_blocking per edit)    {:>9.2} ms\n",
        single.as_secs_f64() * 1e3
    );
    let spot = std::fs::read_to_string(dir.join("f0.txt")).unwrap();
    assert!(spot.contains("MARK-F-0"), "edit chain must have landed");

    // Phase 3: deletes. Each pass needs its own set of files to remove.
    let (serial, _) = run_batch(&tools, deletes(n), 1).await;
    seed(&dir, n, "MARK-A");
    let (concurrent, results) = run_batch(&tools, deletes(n), n).await;
    report("delete_file", n, serial, concurrent, &results);
    let leftovers = std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(leftovers, 0, ".bench must be empty after the deletes");

    // Width sweep: the same 100-call batches at every parallelism width,
    // fresh seed per width, to locate the throughput knee per phase.
    println!("── width sweep (n={n} calls per batch, fresh files per width) ──");
    for width in [1usize, 2, 4, 8, 16, 32, 64, n] {
        seed(&dir, n, "SW-A");
        let (read_t, _) = run_batch(&tools, reads(n), width).await;
        let (edit_t, _) = run_batch(&tools, edits(n, "SW-A", "SW-B"), width).await;
        let (delete_t, _) = run_batch(&tools, deletes(n), width).await;
        println!(
            "   width {width:>3}   read {:>7.2} ms   edit {:>7.2} ms   delete {:>7.2} ms",
            read_t.as_secs_f64() * 1e3,
            edit_t.as_secs_f64() * 1e3,
            delete_t.as_secs_f64() * 1e3,
        );
    }

    // Floor probes: what the edit work costs with no harness at all, to
    // bound how much headroom the tools have left.
    seed(&dir, n, "FL-A");
    let started = Instant::now();
    for i in 0..n {
        let path = dir.join(format!("f{i}.txt"));
        let content = std::fs::read_to_string(&path).unwrap();
        let updated = content.replacen(&format!("FL-A-{i}"), &format!("FL-B-{i}"), 1);
        std::fs::write(&path, updated).unwrap();
    }
    let serial_std = started.elapsed();

    let gate = Arc::new(tokio::sync::Semaphore::new(8));
    let started = Instant::now();
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let path = dir.join(format!("f{i}.txt"));
        let old = format!("FL-B-{i}");
        let new = format!("FL-C-{i}");
        let gate = gate.clone();
        handles.push(tokio::spawn(async move {
            let _permit = gate.acquire_owned().await.unwrap();
            tokio::task::spawn_blocking(move || {
                let content = std::fs::read_to_string(&path).unwrap();
                std::fs::write(&path, content.replacen(&old, &new, 1)).unwrap();
            })
            .await
            .unwrap();
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
    let gated_fused = started.elapsed();

    println!("── floors (100 edits, no harness) ──");
    println!(
        "   single-thread std::fs loop            {:>7.2} ms",
        serial_std.as_secs_f64() * 1e3
    );
    println!(
        "   8-gated fused edits (1 hop/call)      {:>7.2} ms",
        gated_fused.as_secs_f64() * 1e3
    );
    for i in 0..n {
        std::fs::remove_file(dir.join(format!("f{i}.txt"))).ok();
    }

    println!("\nall phases verified: edits landed, .bench emptied by deletes");
}
