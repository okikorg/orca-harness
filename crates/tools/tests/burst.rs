//! Concurrent tool-call burst experiments over large files.
//!
//! Two experiment families, all through the real Dispatcher:
//!
//! - homogeneous: one tool per test, a burst of 100 calls in a single
//!   batch, each call a real action against ~1 MiB fixture files. Every
//!   concurrency class is covered: Parallel fan-out (shell, process,
//!   read_file, list_dir, grep, glob, create_folder, file_info,
//!   subagent), Keyed-by-path (write_file, edit_file, copy_file,
//!   delete_file), multi-key (rename_file), and the independent Keyed
//!   pykernel and bun_repl chains.
//! - heterogeneous: one 100-call batch mixing all 16 shipped tools,
//!   with ordered (Keyed and multi-key) members embedded in the Parallel
//!   majority, plus a same-path Keyed write chain whose final content
//!   proves call-order serialization.
//!
//! Each test prints a `[bench]` line: warm single-call latency, burst
//! wall clock, a 100x-serial estimate, and the derived speedup. Two
//! `[scaling]` tests sweep `max_parallel` over the same burst. Bench:
//!
//! ```sh
//! cargo test --release -p orca-harness-tools --test burst -- --nocapture --test-threads=1
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    CancellationToken, Dispatcher, ExtensionRegistry, ModelResponse, ToolCall, ToolRegistry,
    ToolResult,
};
use orca_harness_tools::{
    fs_admin_tools, BunReplTool, EditFileTool, GlobTool, GrepTool, ListDirTool, ProcessTool,
    PyKernelTool, ReadFileTool, ShellTool, SubagentTool, Workspace, WriteFileTool,
};

const BURST: usize = 100;
const LARGE_BYTES: usize = 1 << 20; // 1 MiB per fixture file
const POOL: usize = 10; // shared read-only large files

static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn temp_ws() -> (Workspace, PathBuf) {
    let n = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("orca-harness-burst-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    (Workspace::new(dir.clone()), dir)
}

fn cleanup(dir: PathBuf) {
    std::fs::remove_dir_all(&dir).ok();
}

/// Exactly LARGE_BYTES of ASCII noise. Contains no "needle".
fn large_noise() -> String {
    let line = "lorem ipsum payload line for concurrent burst experiments 0123456789\n";
    let mut s = String::with_capacity(LARGE_BYTES + line.len());
    while s.len() < LARGE_BYTES {
        s.push_str(line);
    }
    s.truncate(LARGE_BYTES);
    s
}

/// `pool/large_{0..POOL}.txt`, each exactly LARGE_BYTES.
fn write_pool(dir: &Path) {
    let noise = large_noise();
    let pool = dir.join("pool");
    std::fs::create_dir_all(&pool).unwrap();
    for i in 0..POOL {
        std::fs::write(pool.join(format!("large_{i}.txt")), &noise).unwrap();
    }
}

/// `count` files of exactly LARGE_BYTES at `{sub}/{prefix}{i}.txt`.
fn write_large_files(dir: &Path, sub: &str, prefix: &str, count: usize) {
    let noise = large_noise();
    let d = dir.join(sub);
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..count {
        std::fs::write(d.join(format!("{prefix}{i}.txt")), &noise).unwrap();
    }
}

/// Large files each carrying one unique marker line for edit_file.
fn write_marked_files(dir: &Path, sub: &str, count: usize) {
    let noise = large_noise();
    let d = dir.join(sub);
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..count {
        let mut content = noise.clone();
        content.push_str(&format!("\nMARKER_{i}_UNIQUE\n"));
        std::fs::write(d.join(format!("e_{i}.txt")), content).unwrap();
    }
}

/// A flat directory with `count` entries for list_dir.
fn write_wide_dir(dir: &Path, count: usize) {
    let d = dir.join("wide");
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..count {
        std::fs::write(d.join(format!("f_{i}.txt")), "x").unwrap();
    }
}

/// 8 large files, each with 5 "needle" lines appended (40 matches total).
fn write_grep_tree(dir: &Path) {
    let noise = large_noise();
    let d = dir.join("grep_tree");
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..8 {
        let mut content = noise.clone();
        for _ in 0..5 {
            content.push_str("needle marker line\n");
        }
        std::fs::write(d.join(format!("g_{i}.txt")), content).unwrap();
    }
}

/// 40 dirs x 25 .rs files = 1000 glob matches under `tree/`.
fn write_glob_tree(dir: &Path) {
    for m in 0..40 {
        let d = dir.join("tree").join(format!("mod_{m}"));
        std::fs::create_dir_all(&d).unwrap();
        for f in 0..25 {
            std::fs::write(d.join(format!("file_{f}.rs")), "fn x() {}\n").unwrap();
        }
    }
}

/// Full shipped tool set, tuned for the burst sizes used here: reads
/// return the whole 1 MiB, process allows >100 live entries, glob is
/// uncapped for the 1000-file tree.
fn registry(ws: &Workspace) -> ToolRegistry {
    let dir = ws.root().to_string_lossy().into_owned();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ShellTool::local().working_dir(dir.clone())));
    tools.register(Arc::new(
        ProcessTool::local()
            .working_dir(dir.clone())
            .max_processes(2 * BURST),
    ));
    tools.register(Arc::new(
        ReadFileTool::new(ws.clone()).max_bytes(2 * LARGE_BYTES),
    ));
    tools.register(Arc::new(WriteFileTool::new(ws.clone())));
    tools.register(Arc::new(EditFileTool::new(ws.clone())));
    tools.register(Arc::new(ListDirTool::new(ws.clone())));
    tools.register(Arc::new(GrepTool::new(ws.clone())));
    tools.register(Arc::new(GlobTool::new(ws.clone()).max_results(5000)));
    for tool in fs_admin_tools(ws) {
        tools.register(tool);
    }
    tools.register(Arc::new(PyKernelTool::new().working_dir(dir.clone())));
    tools.register(Arc::new(BunReplTool::new().working_dir(dir)));
    tools
}

async fn run(
    tools: &ToolRegistry,
    calls: Vec<ToolCall>,
    max_parallel: usize,
) -> (Vec<ToolResult>, Duration) {
    let started = Instant::now();
    let results = Dispatcher::new()
        .execute(
            calls,
            tools,
            &ExtensionRegistry::new(),
            &CancellationToken::new(),
            None,
            max_parallel,
        )
        .await
        .unwrap();
    (results, started.elapsed())
}

fn assert_all_ok(results: &[ToolResult]) {
    for r in results {
        assert!(
            !r.is_error,
            "{} ({}) failed: {}",
            r.tool_name, r.call_id, r.output
        );
    }
}

/// One warm call through the full dispatch path; its latency anchors the
/// serial estimate.
async fn timed_single(tools: &ToolRegistry, one: ToolCall) -> Duration {
    let (results, wall) = run(tools, vec![one], 1).await;
    assert_all_ok(&results);
    wall
}

fn report(label: &str, n: usize, single: Duration, wall: Duration) {
    let serial_est = single.mul_f64(n as f64);
    let speedup = serial_est.as_secs_f64() / wall.as_secs_f64().max(f64::EPSILON);
    let rate = n as f64 / wall.as_secs_f64().max(f64::EPSILON);
    eprintln!(
        "[bench] {label:<34} single={single:>10.2?}  burst{n}={wall:>10.2?}  \
         serial_est={serial_est:>10.2?}  speedup={speedup:>5.1}x  rate={rate:>7.0}/s"
    );
}

include!("burst/file_tools.rs");
include!("burst/mixed_and_scaling.rs");
