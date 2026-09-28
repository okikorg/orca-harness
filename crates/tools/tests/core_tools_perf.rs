//! Isolated real-filesystem latency and fan-out probe for `edit_file`.
//!
//! Run with:
//! `cargo test --release -p orca-harness-tools --test core_tools_perf -- --nocapture`

use std::sync::Arc;
use std::time::{Duration, Instant};

use orca_harness_core::testing::call;
use orca_harness_core::{
    CancellationToken, Dispatcher, ExtensionRegistry, Tool, ToolCall, ToolRegistry, ToolResult,
};
use orca_harness_tools::{EditFileTool, FileGuard, Workspace};
use serde_json::json;

const CALLS: usize = 32;
const FILE_BYTES: usize = 1 << 20;

fn fixture_content(marker: &str) -> String {
    let line = "latency and concurrency fixture payload 0123456789 abcdefghijklmnopqrstuvwxyz\n";
    let mut content = String::with_capacity(FILE_BYTES + marker.len() + 2);
    while content.len() < FILE_BYTES {
        content.push_str(line);
    }
    content.truncate(FILE_BYTES);
    content.push('\n');
    content.push_str(marker);
    content.push('\n');
    content
}

fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "orca-harness-{label}-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn prepare(dir: &std::path::Path, prefix: &str) {
    for index in 0..CALLS {
        let marker = format!("MARKER_{index}");
        std::fs::write(
            dir.join(format!("{prefix}_{index}.txt")),
            fixture_content(&marker),
        )
        .unwrap();
    }
}

async fn dispatch(
    tool: Arc<dyn Tool>,
    calls: Vec<ToolCall>,
    max_parallel: usize,
) -> (Vec<ToolResult>, Duration) {
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let started = Instant::now();
    let results = Dispatcher::new()
        .execute(
            calls,
            &registry,
            &ExtensionRegistry::new(),
            &CancellationToken::new(),
            None,
            max_parallel,
        )
        .await
        .unwrap();
    (results, started.elapsed())
}

/// One single-form `edit_file` call per file: the common case.
fn edit_calls(prefix: &str) -> Vec<ToolCall> {
    (0..CALLS)
        .map(|index| {
            call(
                &format!("edit-{index}"),
                "edit_file",
                json!({
                    "path": format!("{prefix}_{index}.txt"),
                    "old": format!("MARKER_{index}"),
                    "new": format!("EDITED_{index}")
                }),
            )
        })
        .collect()
}

fn batch_call(id: &str, edits: Vec<serde_json::Value>) -> ToolCall {
    call(id, "edit_file", json!({"edits": edits}))
}

fn batch_files_call(prefix: &str) -> ToolCall {
    let edits = (0..CALLS)
        .map(|index| {
            json!({
                "path": format!("{prefix}_{index}.txt"),
                "old": format!("MARKER_{index}"),
                "new": format!("EDITED_{index}"),
            })
        })
        .collect();
    batch_call("edit-batch", edits)
}

fn append_batch_call(prefix: &str) -> ToolCall {
    let edits = (0..CALLS)
        .map(|index| {
            json!({
                "path": format!("{prefix}_{index}.txt"),
                "operation": "append",
                "content": format!("APPENDED_{index}\n"),
            })
        })
        .collect();
    batch_call("edit-append", edits)
}

fn prepare_repeated(dir: &std::path::Path, name: &str) {
    let mut content = fixture_content("REPEATED_START");
    for index in 0..CALLS {
        content.push_str(&format!("MARKER_{index:03}\n"));
    }
    std::fs::write(dir.join(name), content).unwrap();
}

fn repeated_call(name: &str) -> ToolCall {
    let edits = (0..CALLS)
        .map(|index| {
            json!({
                "path": name,
                "old": format!("MARKER_{index:03}"),
                "new": format!("EDITED_{index:03}"),
            })
        })
        .collect();
    batch_call("edit-repeated", edits)
}

fn tool(dir: &std::path::Path) -> EditFileTool {
    EditFileTool::new(Workspace::new(dir.to_path_buf()))
}

fn assert_results(results: &[ToolResult]) {
    assert_eq!(results.len(), CALLS);
    assert!(
        results.iter().all(|result| !result.is_error),
        "tool failure: {results:?}"
    );
}

fn assert_single_result(results: &[ToolResult], files: usize) {
    assert_eq!(results.len(), 1);
    assert!(!results[0].is_error, "tool failure: {results:?}");
    assert_eq!(results[0].output["editsApplied"], CALLS);
    assert_eq!(results[0].output["filesChanged"], files);
}

fn assert_markers(dir: &std::path::Path, prefix: &str) {
    for index in 0..CALLS {
        let content = std::fs::read_to_string(dir.join(format!("{prefix}_{index}.txt"))).unwrap();
        assert!(content.contains(&format!("EDITED_{index}")));
        assert!(!content.contains(&format!("MARKER_{index}")));
    }
}

fn assert_repeated_markers(dir: &std::path::Path, name: &str) {
    let content = std::fs::read_to_string(dir.join(name)).unwrap();
    for index in 0..CALLS {
        assert!(content.contains(&format!("EDITED_{index:03}")));
        assert!(!content.contains(&format!("MARKER_{index:03}")));
    }
}

fn assert_appended(dir: &std::path::Path, prefix: &str) {
    for index in 0..CALLS {
        let content = std::fs::read_to_string(dir.join(format!("{prefix}_{index}.txt"))).unwrap();
        assert!(
            content.ends_with(&format!("MARKER_{index}\nAPPENDED_{index}\n")),
            "append missing from {prefix}_{index}.txt"
        );
    }
}

fn report_parallel(scenario: &str, serial: Duration, concurrent: Duration) {
    println!(
        "[core-tools-bench] {}",
        json!({
            "tool": "edit_file",
            "scenario": scenario,
            "calls": CALLS,
            "files": CALLS,
            "operations": CALLS,
            "bytes_per_file": FILE_BYTES,
            "serial_seconds": serial.as_secs_f64(),
            "single_average_seconds": serial.as_secs_f64() / CALLS as f64,
            "wall_seconds": concurrent.as_secs_f64(),
            "speedup": serial.as_secs_f64() / concurrent.as_secs_f64(),
        })
    );
}

fn report_single(scenario: &str, files: usize, wall: Duration) {
    println!(
        "[core-tools-bench] {}",
        json!({
            "tool": "edit_file",
            "scenario": scenario,
            "calls": 1,
            "files": files,
            "operations": CALLS,
            "bytes_per_file": FILE_BYTES,
            "wall_seconds": wall.as_secs_f64(),
        })
    );
}

/// Serial versus fully admitted distinct-file edits, with or without the
/// production [`FileGuard`].
async fn distinct(scenario: &str, guarded: bool) {
    let serial_dir = temp_dir(&format!("edit-{scenario}-serial"));
    let concurrent_dir = temp_dir(&format!("edit-{scenario}-concurrent"));
    prepare(&serial_dir, "serial");
    prepare(&concurrent_dir, "concurrent");
    let (serial_guard, concurrent_guard) = (FileGuard::new(), FileGuard::new());
    let build = |dir: &std::path::Path, guard: &FileGuard| -> Arc<dyn Tool> {
        match guarded {
            true => Arc::new(tool(dir).guard(guard.clone())),
            false => Arc::new(tool(dir)),
        }
    };

    let (serial_results, serial) =
        dispatch(build(&serial_dir, &serial_guard), edit_calls("serial"), 1).await;
    let (concurrent_results, concurrent) = dispatch(
        build(&concurrent_dir, &concurrent_guard),
        edit_calls("concurrent"),
        CALLS,
    )
    .await;

    assert_results(&serial_results);
    assert_results(&concurrent_results);
    assert_markers(&serial_dir, "serial");
    assert_markers(&concurrent_dir, "concurrent");
    if guarded {
        assert_eq!(serial_guard.len(), CALLS);
        assert_eq!(concurrent_guard.len(), CALLS);
    }
    report_parallel(scenario, serial, concurrent);
    std::fs::remove_dir_all(serial_dir).ok();
    std::fs::remove_dir_all(concurrent_dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn distinct_file_latency_accuracy_and_concurrency() {
    distinct("distinct", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn guarded_distinct_file_latency_and_concurrency() {
    distinct("guarded_distinct", true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn one_call_edits_many_files() {
    let dir = temp_dir("edit-batch-files");
    prepare(&dir, "batch");
    let (results, wall) = dispatch(Arc::new(tool(&dir)), vec![batch_files_call("batch")], 1).await;
    assert_single_result(&results, CALLS);
    assert_markers(&dir, "batch");
    report_single("batch_files", CALLS, wall);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn one_file_receives_many_ordered_edits() {
    let dir = temp_dir("edit-repeated");
    prepare_repeated(&dir, "repeated.txt");
    let (results, wall) =
        dispatch(Arc::new(tool(&dir)), vec![repeated_call("repeated.txt")], 1).await;
    assert_single_result(&results, 1);
    assert_repeated_markers(&dir, "repeated.txt");
    report_single("repeated_file", 1, wall);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn one_call_appends_to_many_files() {
    let dir = temp_dir("edit-append-files");
    prepare(&dir, "append");
    let (results, wall) =
        dispatch(Arc::new(tool(&dir)), vec![append_batch_call("append")], 1).await;
    assert_single_result(&results, CALLS);
    assert_appended(&dir, "append");
    report_single("append_files", CALLS, wall);
    std::fs::remove_dir_all(dir).ok();
}
