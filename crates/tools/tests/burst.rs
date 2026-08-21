//! Concurrent tool-call burst experiments over large files.
//!
//! Two experiment families, all through the real Dispatcher:
//!
//! - homogeneous: one tool per test, a burst of 100 calls in a single
//!   batch, each call a real action against ~1 MiB fixture files. Every
//!   concurrency class is covered: Parallel fan-out (shell, process,
//!   read_file, list_dir, grep, glob, create_folder, file_info,
//!   subagent), Keyed-by-path (write_file, edit_file, copy_file,
//!   delete_file), multi-key (rename_file), and the Keyed pykernel chain.
//! - heterogeneous: one 100-call batch mixing all 15 shipped tools,
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
    fs_admin_tools, EditFileTool, GlobTool, GrepTool, ListDirTool, ProcessTool, PyKernelTool,
    ReadFileTool, ShellTool, SubagentTool, Workspace, WriteFileTool,
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
    tools.register(Arc::new(PyKernelTool::new().working_dir(dir)));
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

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

// --------------------------------------------------------------------
// Homogeneous bursts: 100 calls of one tool, real action, large files.
// --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_shell_burst() {
    let (ws, dir) = temp_ws();
    write_pool(&dir);
    let tools = registry(&ws);

    let single = timed_single(
        &tools,
        call(
            "warm",
            "shell",
            json!({"command": "wc -c < pool/large_0.txt"}),
        ),
    )
    .await;
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| {
            call(
                &format!("c{i}"),
                "shell",
                json!({"command": format!("wc -c < pool/large_{}.txt", i % POOL)}),
            )
        })
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    for r in &results {
        let n: usize = r.output["stdout"].as_str().unwrap().trim().parse().unwrap();
        assert_eq!(n, LARGE_BYTES);
        assert_eq!(r.output["success"], true);
    }
    report("shell (wc -c, 1MiB)", BURST, single, wall);
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_process_spawn_burst() {
    let (ws, dir) = temp_ws();
    write_pool(&dir);
    let tools = registry(&ws);

    let single = timed_single(
        &tools,
        call(
            "warm",
            "process",
            json!({"action": "spawn", "command": "cat pool/large_0.txt"}),
        ),
    )
    .await;
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| {
            call(
                &format!("c{i}"),
                "process",
                json!({"action": "spawn", "command": format!("cat pool/large_{}.txt", i % POOL)}),
            )
        })
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    let ids: HashSet<&str> = results
        .iter()
        .map(|r| r.output["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), BURST, "every spawn must get a distinct id");
    report("process spawn (cat 1MiB)", BURST, single, wall);
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_read_file_burst() {
    let (ws, dir) = temp_ws();
    write_pool(&dir);
    let tools = registry(&ws);

    let single = timed_single(
        &tools,
        call("warm", "read_file", json!({"path": "pool/large_0.txt"})),
    )
    .await;
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| {
            call(
                &format!("c{i}"),
                "read_file",
                json!({"path": format!("pool/large_{}.txt", i % POOL)}),
            )
        })
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    for r in &results {
        assert_eq!(r.output["bytes"].as_u64().unwrap() as usize, LARGE_BYTES);
        assert_eq!(r.output["truncated"], false);
        assert_eq!(r.output["content"].as_str().unwrap().len(), LARGE_BYTES);
    }
    report("read_file (1MiB)", BURST, single, wall);
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_write_file_burst() {
    let (ws, dir) = temp_ws();
    let tools = registry(&ws);
    let content = large_noise();

    let single = timed_single(
        &tools,
        call(
            "warm",
            "write_file",
            json!({"path": "out/warm.txt", "content": content}),
        ),
    )
    .await;
    // Distinct paths: Keyed(file:path) with no shared key, so the whole
    // burst fans out.
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| {
            call(
                &format!("c{i}"),
                "write_file",
                json!({"path": format!("out/w_{i}.txt"), "content": content}),
            )
        })
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    for r in &results {
        assert_eq!(
            r.output["bytesWritten"].as_u64().unwrap() as usize,
            LARGE_BYTES
        );
    }
    for i in [0, BURST / 2, BURST - 1] {
        let meta = std::fs::metadata(dir.join(format!("out/w_{i}.txt"))).unwrap();
        assert_eq!(meta.len() as usize, LARGE_BYTES);
    }
    report("write_file (1MiB, distinct keys)", BURST, single, wall);
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_edit_file_burst() {
    let (ws, dir) = temp_ws();
    write_marked_files(&dir, "edit", BURST);
    std::fs::write(dir.join("edit/warm.txt"), "WARM_MARKER\n").unwrap();
    let tools = registry(&ws);

    let single = timed_single(
        &tools,
        call(
            "warm",
            "edit_file",
            json!({"path": "edit/warm.txt", "old": "WARM_MARKER", "new": "WARM_DONE"}),
        ),
    )
    .await;
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| {
            call(
                &format!("c{i}"),
                "edit_file",
                json!({
                    "path": format!("edit/e_{i}.txt"),
                    "old": format!("MARKER_{i}_UNIQUE"),
                    "new": format!("EDITED_{i}"),
                }),
            )
        })
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    for r in &results {
        assert_eq!(r.output["replacements"], 1);
    }
    let edited = std::fs::read_to_string(dir.join("edit/e_0.txt")).unwrap();
    assert!(edited.contains("EDITED_0") && !edited.contains("MARKER_0_UNIQUE"));
    report("edit_file (1MiB, distinct keys)", BURST, single, wall);
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_list_dir_burst() {
    let (ws, dir) = temp_ws();
    write_wide_dir(&dir, 1000);
    let tools = registry(&ws);

    let single = timed_single(&tools, call("warm", "list_dir", json!({"path": "wide"}))).await;
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| call(&format!("c{i}"), "list_dir", json!({"path": "wide"})))
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    for r in &results {
        assert_eq!(r.output["entries"].as_array().unwrap().len(), 1000);
    }
    report("list_dir (1000 entries)", BURST, single, wall);
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_grep_burst() {
    let (ws, dir) = temp_ws();
    write_grep_tree(&dir);
    let tools = registry(&ws);

    let single = timed_single(
        &tools,
        call(
            "warm",
            "grep",
            json!({"query": "needle", "path": "grep_tree"}),
        ),
    )
    .await;
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| {
            call(
                &format!("c{i}"),
                "grep",
                json!({"query": "needle", "path": "grep_tree"}),
            )
        })
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    for r in &results {
        assert_eq!(r.output["matches"].as_array().unwrap().len(), 40);
        assert_eq!(r.output["truncated"], false);
    }
    report("grep (8 x 1MiB full scan)", BURST, single, wall);
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_glob_burst() {
    let (ws, dir) = temp_ws();
    write_glob_tree(&dir);
    let tools = registry(&ws);

    let single = timed_single(
        &tools,
        call("warm", "glob", json!({"pattern": "*.rs", "path": "tree"})),
    )
    .await;
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| {
            call(
                &format!("c{i}"),
                "glob",
                json!({"pattern": "*.rs", "path": "tree"}),
            )
        })
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    for r in &results {
        assert_eq!(r.output["matches"].as_array().unwrap().len(), 1000);
        assert_eq!(r.output["truncated"], false);
    }
    report("glob (1000-file tree walk)", BURST, single, wall);
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_copy_file_burst() {
    let (ws, dir) = temp_ws();
    write_pool(&dir);
    let tools = registry(&ws);

    let single = timed_single(
        &tools,
        call(
            "warm",
            "copy_file",
            json!({"from": "pool/large_0.txt", "to": "out/warm.bin"}),
        ),
    )
    .await;
    // Keyed by destination; all destinations distinct.
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| {
            call(
                &format!("c{i}"),
                "copy_file",
                json!({
                    "from": format!("pool/large_{}.txt", i % POOL),
                    "to": format!("out/copy_{i}.bin"),
                }),
            )
        })
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    for r in &results {
        assert_eq!(
            r.output["bytesCopied"].as_u64().unwrap() as usize,
            LARGE_BYTES
        );
    }
    report("copy_file (1MiB, distinct keys)", BURST, single, wall);
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_rename_file_burst() {
    let (ws, dir) = temp_ws();
    write_large_files(&dir, "mv", "m_", BURST);
    std::fs::write(dir.join("mv/warm.txt"), "w").unwrap();
    let tools = registry(&ws);

    let single = timed_single(
        &tools,
        call(
            "warm",
            "rename_file",
            json!({"from": "mv/warm.txt", "to": "mv/warm_done.txt"}),
        ),
    )
    .await;
    // rename_file keys on both paths; these renames touch disjoint
    // path pairs, so the burst fans out like other keyed tools.
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| {
            call(
                &format!("c{i}"),
                "rename_file",
                json!({
                    "from": format!("mv/m_{i}.txt"),
                    "to": format!("mv/renamed_{i}.txt"),
                }),
            )
        })
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    for i in 0..BURST {
        assert!(dir.join(format!("mv/renamed_{i}.txt")).exists());
        assert!(!dir.join(format!("mv/m_{i}.txt")).exists());
    }
    report("rename_file (1MiB, multi-key)", BURST, single, wall);
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_delete_file_burst() {
    let (ws, dir) = temp_ws();
    write_large_files(&dir, "del", "d_", BURST);
    std::fs::write(dir.join("del/warm.txt"), "w").unwrap();
    let tools = registry(&ws);

    let single = timed_single(
        &tools,
        call("warm", "delete_file", json!({"path": "del/warm.txt"})),
    )
    .await;
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| {
            call(
                &format!("c{i}"),
                "delete_file",
                json!({"path": format!("del/d_{i}.txt")}),
            )
        })
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    assert_eq!(std::fs::read_dir(dir.join("del")).unwrap().count(), 0);
    report("delete_file (1MiB, distinct keys)", BURST, single, wall);
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_create_folder_burst() {
    let (ws, dir) = temp_ws();
    let tools = registry(&ws);

    let single = timed_single(
        &tools,
        call("warm", "create_folder", json!({"path": "made/warm"})),
    )
    .await;
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| {
            call(
                &format!("c{i}"),
                "create_folder",
                json!({"path": format!("made/d_{i}/nested")}),
            )
        })
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    for i in 0..BURST {
        assert!(dir.join(format!("made/d_{i}/nested")).is_dir());
    }
    report("create_folder (nested)", BURST, single, wall);
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_file_info_burst() {
    let (ws, dir) = temp_ws();
    write_pool(&dir);
    let tools = registry(&ws);

    let single = timed_single(
        &tools,
        call("warm", "file_info", json!({"path": "pool/large_0.txt"})),
    )
    .await;
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| {
            call(
                &format!("c{i}"),
                "file_info",
                json!({"path": format!("pool/large_{}.txt", i % POOL)}),
            )
        })
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    for r in &results {
        assert_eq!(r.output["exists"], true);
        assert_eq!(r.output["kind"], "file");
        assert_eq!(
            r.output["sizeBytes"].as_u64().unwrap() as usize,
            LARGE_BYTES
        );
    }
    report("file_info (1MiB stat)", BURST, single, wall);
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_pykernel_burst_keyed_chain() {
    if !python3_available() {
        eprintln!("skipping: python3 not found on PATH");
        return;
    }
    let (ws, dir) = temp_ws();
    write_pool(&dir);
    let tools = registry(&ws);

    // Warm spawn of the kernel; the burst then measures steady state.
    let single = timed_single(
        &tools,
        call("warm", "pykernel", json!({"code": "print('warm')"})),
    )
    .await;

    // Each call reads 1 MiB and folds it into a persistent accumulator.
    // pykernel is Keyed, so the batch runs as one chain in call order —
    // result i must report exactly (i+1) files' worth of bytes, which
    // proves both the ordering and the state persistence across a
    // 100-call burst.
    let code = "data = open('pool/large_0.txt', 'rb').read()\n\
                total = globals().get('total', 0) + len(data)\n\
                print(total)";
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| call(&format!("c{i}"), "pykernel", json!({"code": code})))
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r.output["state"], "ok");
        assert_eq!(
            r.output["output"].as_str().unwrap().trim(),
            ((i + 1) * LARGE_BYTES).to_string(),
            "call {i} observed an out-of-order or lossy accumulator"
        );
    }
    report("pykernel (1MiB read, Keyed)", BURST, single, wall);
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn homogeneous_subagent_burst() {
    let (ws, dir) = temp_ws();
    write_pool(&dir);
    let mut tools = registry(&ws);
    // One scripted response per spawn (warmup + burst); every inner
    // agent pops one Final and returns.
    let model = Arc::new(ScriptedModel::new(
        (0..=BURST)
            .map(|_| ModelResponse::final_text("done"))
            .collect(),
    ));
    tools.register(Arc::new(SubagentTool::new(model, &ws)));

    let single = timed_single(&tools, call("warm", "subagent", json!({"task": "warm"}))).await;
    let batch: Vec<ToolCall> = (0..BURST)
        .map(|i| {
            call(
                &format!("c{i}"),
                "subagent",
                json!({"task": format!("task {i}")}),
            )
        })
        .collect();
    let (results, wall) = run(&tools, batch, BURST).await;
    assert_all_ok(&results);
    for r in &results {
        assert_eq!(r.output["answer"], "done");
    }
    report("subagent (scripted agent)", BURST, single, wall);
    cleanup(dir);
}

// --------------------------------------------------------------------
// Heterogeneous burst: one 100-call batch across all 15 tools.
// --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn heterogeneous_burst_100_all_tools() {
    let (ws, dir) = temp_ws();
    write_pool(&dir);
    write_marked_files(&dir, "edit", 8);
    write_wide_dir(&dir, 1000);
    write_grep_tree(&dir);
    write_glob_tree(&dir);
    write_large_files(&dir, "mv", "m_", 4);
    write_large_files(&dir, "del", "d_", 6);

    let mut tools = registry(&ws);
    let model = Arc::new(ScriptedModel::new(
        (0..2).map(|_| ModelResponse::final_text("done")).collect(),
    ));
    tools.register(Arc::new(SubagentTool::new(model, &ws)));

    let python = python3_available();
    let content = large_noise();
    let mut seq = 0usize;
    let mut id = move || {
        let s = format!("h{seq}");
        seq += 1;
        s
    };

    let mut batch: Vec<ToolCall> = Vec::with_capacity(BURST);
    // 8 shell
    for i in 0..8 {
        batch.push(call(
            &id(),
            "shell",
            json!({"command": format!("wc -c < pool/large_{}.txt", i % POOL)}),
        ));
    }
    // 6 process spawn
    for i in 0..6 {
        batch.push(call(
            &id(),
            "process",
            json!({"action": "spawn", "command": format!("cat pool/large_{}.txt", i % POOL)}),
        ));
    }
    // 12 read_file
    for i in 0..12 {
        batch.push(call(
            &id(),
            "read_file",
            json!({"path": format!("pool/large_{}.txt", i % POOL)}),
        ));
    }
    // 8 write_file, distinct paths
    for i in 0..8 {
        batch.push(call(
            &id(),
            "write_file",
            json!({"path": format!("out/w_{i}.txt"), "content": content}),
        ));
    }
    // 3 write_file to the SAME path: a Keyed chain whose call order must
    // decide the final content.
    for v in 1..=3 {
        batch.push(call(
            &id(),
            "write_file",
            json!({"path": "keyed/hot.txt", "content": format!("v{v}")}),
        ));
    }
    // 8 edit_file
    for i in 0..8 {
        batch.push(call(
            &id(),
            "edit_file",
            json!({
                "path": format!("edit/e_{i}.txt"),
                "old": format!("MARKER_{i}_UNIQUE"),
                "new": format!("EDITED_{i}"),
            }),
        ));
    }
    // 6 list_dir
    for _ in 0..6 {
        batch.push(call(&id(), "list_dir", json!({"path": "wide"})));
    }
    // 5 grep
    for _ in 0..5 {
        batch.push(call(
            &id(),
            "grep",
            json!({"query": "needle", "path": "grep_tree"}),
        ));
    }
    // 6 glob
    for _ in 0..6 {
        batch.push(call(
            &id(),
            "glob",
            json!({"pattern": "*.rs", "path": "tree"}),
        ));
    }
    // 8 copy_file
    for i in 0..8 {
        batch.push(call(
            &id(),
            "copy_file",
            json!({
                "from": format!("pool/large_{}.txt", i % POOL),
                "to": format!("out/copy_{i}.bin"),
            }),
        ));
    }
    // 4 rename_file (multi-key members)
    for i in 0..4 {
        batch.push(call(
            &id(),
            "rename_file",
            json!({"from": format!("mv/m_{i}.txt"), "to": format!("mv/renamed_{i}.txt")}),
        ));
    }
    // 6 delete_file
    for i in 0..6 {
        batch.push(call(
            &id(),
            "delete_file",
            json!({"path": format!("del/d_{i}.txt")}),
        ));
    }
    // 6 create_folder
    for i in 0..6 {
        batch.push(call(
            &id(),
            "create_folder",
            json!({"path": format!("made/d_{i}")}),
        ));
    }
    // 8 file_info
    for i in 0..8 {
        batch.push(call(
            &id(),
            "file_info",
            json!({"path": format!("pool/large_{}.txt", i % POOL)}),
        ));
    }
    // 4 pykernel (one Keyed chain), or 4 more file_info without python3
    if python {
        let code = "data = open('pool/large_0.txt', 'rb').read()\n\
                    total = globals().get('total', 0) + len(data)\n\
                    print(total)";
        for _ in 0..4 {
            batch.push(call(&id(), "pykernel", json!({"code": code})));
        }
    } else {
        for i in 0..4 {
            batch.push(call(
                &id(),
                "file_info",
                json!({"path": format!("pool/large_{}.txt", i % POOL)}),
            ));
        }
    }
    // 2 subagent
    for i in 0..2 {
        batch.push(call(
            &id(),
            "subagent",
            json!({"task": format!("task {i}")}),
        ));
    }
    assert_eq!(batch.len(), BURST);

    let (results, wall) = run(&tools, batch, BURST).await;
    assert_eq!(results.len(), BURST);
    assert_all_ok(&results);

    // Keyed same-path chain resolved in call order: last write wins.
    assert_eq!(
        std::fs::read_to_string(dir.join("keyed/hot.txt")).unwrap(),
        "v3"
    );
    // Ordered members really ran: renames landed, kernel accumulated in
    // call order alongside 90+ concurrent neighbours.
    for i in 0..4 {
        assert!(dir.join(format!("mv/renamed_{i}.txt")).exists());
    }
    for i in 0..6 {
        assert!(!dir.join(format!("del/d_{i}.txt")).exists());
    }
    if python {
        let totals: Vec<&str> = results
            .iter()
            .filter(|r| r.tool_name == "pykernel")
            .map(|r| r.output["output"].as_str().unwrap().trim())
            .collect();
        let expected: Vec<String> = (1..=4).map(|i| (i * LARGE_BYTES).to_string()).collect();
        assert_eq!(totals, expected);
    }
    for r in results.iter().filter(|r| r.tool_name == "subagent") {
        assert_eq!(r.output["answer"], "done");
    }

    let rate = BURST as f64 / wall.as_secs_f64().max(f64::EPSILON);
    eprintln!(
        "[bench] heterogeneous 15-tool mix          burst{BURST}={wall:>10.2?}  rate={rate:>7.0}/s"
    );
    cleanup(dir);
}

// --------------------------------------------------------------------
// Scaling: the same 100-call burst under different max_parallel caps.
// --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn scaling_read_file_max_parallel() {
    let (ws, dir) = temp_ws();
    write_pool(&dir);
    let tools = registry(&ws);

    eprintln!("[scaling] read_file: {BURST} x 1MiB reads, sweeping max_parallel");
    for mp in [1usize, 4, 16, BURST] {
        let batch: Vec<ToolCall> = (0..BURST)
            .map(|i| {
                call(
                    &format!("r{mp}_{i}"),
                    "read_file",
                    json!({"path": format!("pool/large_{}.txt", i % POOL)}),
                )
            })
            .collect();
        let (results, wall) = run(&tools, batch, mp).await;
        assert_all_ok(&results);
        let rate = BURST as f64 / wall.as_secs_f64().max(f64::EPSILON);
        eprintln!("  max_parallel={mp:<4} wall={wall:>10.2?}  rate={rate:>7.0}/s");
    }
    cleanup(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn scaling_shell_max_parallel() {
    let (ws, dir) = temp_ws();
    write_pool(&dir);
    let tools = registry(&ws);

    eprintln!("[scaling] shell: {BURST} x `wc -c` over 1MiB, sweeping max_parallel");
    for mp in [1usize, 4, 16, BURST] {
        let batch: Vec<ToolCall> = (0..BURST)
            .map(|i| {
                call(
                    &format!("s{mp}_{i}"),
                    "shell",
                    json!({"command": format!("wc -c < pool/large_{}.txt", i % POOL)}),
                )
            })
            .collect();
        let (results, wall) = run(&tools, batch, mp).await;
        assert_all_ok(&results);
        let rate = BURST as f64 / wall.as_secs_f64().max(f64::EPSILON);
        eprintln!("  max_parallel={mp:<4} wall={wall:>10.2?}  rate={rate:>7.0}/s");
    }
    cleanup(dir);
}
