//! Isolated real-filesystem latency and fan-out probe for the two core mutation tools.
//!
//! Run with:
//! `cargo test --release -p orca-harness-tools --test core_tools_perf -- --nocapture`

use std::sync::Arc;
use std::time::{Duration, Instant};

use orca_harness_core::testing::call;
use orca_harness_core::{
    CancellationToken, Dispatcher, ExtensionRegistry, Tool, ToolCall, ToolRegistry, ToolResult,
};
use orca_harness_tools::{ApplyPatchTool, FileGuard, MultiEditTool, Workspace};
use serde_json::json;

const CALLS: usize = 32;
const FILE_BYTES: usize = 1 << 20;

type GuardedCase = (
    Arc<dyn Tool>,
    Arc<dyn Tool>,
    Vec<ToolCall>,
    Vec<ToolCall>,
    &'static str,
);

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

fn multi_calls(prefix: &str) -> Vec<ToolCall> {
    (0..CALLS)
        .map(|index| {
            call(
                &format!("multi-{index}"),
                "multi_edit",
                json!({"edits": [{
                    "path": format!("{prefix}_{index}.txt"),
                    "old": format!("MARKER_{index}"),
                    "new": format!("EDITED_{index}")
                }]}),
            )
        })
        .collect()
}

fn patch_calls(prefix: &str) -> Vec<ToolCall> {
    (0..CALLS)
        .map(|index| {
            let patch = format!(
                "*** Begin Patch\n*** Update File: {prefix}_{index}.txt\n@@\n-MARKER_{index}\n+PATCHED_{index}\n*** End Patch"
            );
            call(
                &format!("patch-{index}"),
                "apply_patch",
                json!({"patch": patch}),
            )
        })
        .collect()
}

fn multi_batch_call(prefix: &str) -> ToolCall {
    let edits: Vec<_> = (0..CALLS)
        .map(|index| {
            json!({
                "path": format!("{prefix}_{index}.txt"),
                "old": format!("MARKER_{index}"),
                "new": format!("EDITED_{index}"),
            })
        })
        .collect();
    call("multi-batch", "multi_edit", json!({"edits": edits}))
}

fn patch_batch_call(prefix: &str) -> ToolCall {
    let mut patch = String::from("*** Begin Patch\n");
    for index in 0..CALLS {
        patch.push_str(&format!(
            "*** Update File: {prefix}_{index}.txt\n@@\n-MARKER_{index}\n+PATCHED_{index}\n"
        ));
    }
    patch.push_str("*** End Patch");
    call("patch-batch", "apply_patch", json!({"patch": patch}))
}

fn multi_append_batch_call(prefix: &str) -> ToolCall {
    let edits: Vec<_> = (0..CALLS)
        .map(|index| {
            json!({
                "path": format!("{prefix}_{index}.txt"),
                "operation": "append",
                "content": format!("APPENDED_{index}\n"),
            })
        })
        .collect();
    call("multi-append", "multi_edit", json!({"edits": edits}))
}

fn patch_append_batch_call(prefix: &str) -> ToolCall {
    let mut patch = String::from("*** Begin Patch\n");
    for index in 0..CALLS {
        patch.push_str(&format!(
            "*** Update File: {prefix}_{index}.txt\n@@\n MARKER_{index}\n+APPENDED_{index}\n"
        ));
    }
    patch.push_str("*** End Patch");
    call("patch-append", "apply_patch", json!({"patch": patch}))
}

fn prepare_repeated(dir: &std::path::Path, name: &str) {
    let mut content = fixture_content("REPEATED_START");
    for index in 0..CALLS {
        content.push_str(&format!("MARKER_{index:03}\n"));
    }
    std::fs::write(dir.join(name), content).unwrap();
}

fn multi_repeated_call(name: &str) -> ToolCall {
    let edits: Vec<_> = (0..CALLS)
        .map(|index| {
            json!({
                "path": name,
                "old": format!("MARKER_{index:03}"),
                "new": format!("EDITED_{index:03}"),
            })
        })
        .collect();
    call("multi-repeated", "multi_edit", json!({"edits": edits}))
}

fn patch_repeated_call(name: &str) -> ToolCall {
    let mut patch = format!("*** Begin Patch\n*** Update File: {name}\n");
    for index in 0..CALLS {
        patch.push_str(&format!("@@\n-MARKER_{index:03}\n+PATCHED_{index:03}\n"));
    }
    patch.push_str("*** End Patch");
    call("patch-repeated", "apply_patch", json!({"patch": patch}))
}

fn assert_results(results: &[ToolResult]) {
    assert_eq!(results.len(), CALLS);
    assert!(
        results.iter().all(|result| !result.is_error),
        "tool failure: {results:?}"
    );
}

fn assert_single_result(results: &[ToolResult]) {
    assert_eq!(results.len(), 1);
    assert!(!results[0].is_error, "tool failure: {results:?}");
}

fn assert_markers(dir: &std::path::Path, prefix: &str, replacement: &str) {
    for index in 0..CALLS {
        let content = std::fs::read_to_string(dir.join(format!("{prefix}_{index}.txt"))).unwrap();
        assert!(content.contains(&format!("{replacement}_{index}")));
        assert!(!content.contains(&format!("MARKER_{index}")));
    }
}

fn assert_repeated_markers(dir: &std::path::Path, name: &str, replacement: &str) {
    let content = std::fs::read_to_string(dir.join(name)).unwrap();
    for index in 0..CALLS {
        assert!(content.contains(&format!("{replacement}_{index:03}")));
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

fn report_parallel(name: &str, scenario: &str, serial: Duration, concurrent: Duration) {
    println!(
        "[core-tools-bench] {}",
        json!({
            "tool": name,
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

fn report_single(name: &str, scenario: &str, files: usize, wall: Duration) {
    println!(
        "[core-tools-bench] {}",
        json!({
            "tool": name,
            "scenario": scenario,
            "calls": 1,
            "files": files,
            "operations": CALLS,
            "bytes_per_file": FILE_BYTES,
            "wall_seconds": wall.as_secs_f64(),
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn isolated_multi_edit_latency_accuracy_and_concurrency() {
    let serial_dir = temp_dir("multi-edit-serial");
    let concurrent_dir = temp_dir("multi-edit-concurrent");
    prepare(&serial_dir, "serial");
    prepare(&concurrent_dir, "concurrent");

    let (serial_results, serial) = dispatch(
        Arc::new(MultiEditTool::new(Workspace::new(serial_dir.clone()))),
        multi_calls("serial"),
        1,
    )
    .await;
    let (concurrent_results, concurrent) = dispatch(
        Arc::new(MultiEditTool::new(Workspace::new(concurrent_dir.clone()))),
        multi_calls("concurrent"),
        CALLS,
    )
    .await;

    assert_results(&serial_results);
    assert_results(&concurrent_results);
    assert_markers(&serial_dir, "serial", "EDITED");
    assert_markers(&concurrent_dir, "concurrent", "EDITED");
    report_parallel("multi_edit", "distinct", serial, concurrent);
    std::fs::remove_dir_all(serial_dir).ok();
    std::fs::remove_dir_all(concurrent_dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn isolated_apply_patch_latency_accuracy_and_concurrency() {
    let serial_dir = temp_dir("apply-patch-serial");
    let concurrent_dir = temp_dir("apply-patch-concurrent");
    prepare(&serial_dir, "serial");
    prepare(&concurrent_dir, "concurrent");

    let (serial_results, serial) = dispatch(
        Arc::new(ApplyPatchTool::new(Workspace::new(serial_dir.clone()))),
        patch_calls("serial"),
        1,
    )
    .await;
    let (concurrent_results, concurrent) = dispatch(
        Arc::new(ApplyPatchTool::new(Workspace::new(concurrent_dir.clone()))),
        patch_calls("concurrent"),
        CALLS,
    )
    .await;

    assert_results(&serial_results);
    assert_results(&concurrent_results);
    assert_markers(&serial_dir, "serial", "PATCHED");
    assert_markers(&concurrent_dir, "concurrent", "PATCHED");
    report_parallel("apply_patch", "distinct", serial, concurrent);
    std::fs::remove_dir_all(serial_dir).ok();
    std::fs::remove_dir_all(concurrent_dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn guarded_distinct_file_latency_and_concurrency() {
    for name in ["multi_edit", "apply_patch"] {
        let serial_dir = temp_dir(&format!("{name}-guarded-serial"));
        let concurrent_dir = temp_dir(&format!("{name}-guarded-concurrent"));
        prepare(&serial_dir, "serial");
        prepare(&concurrent_dir, "concurrent");

        let serial_guard = FileGuard::new();
        let concurrent_guard = FileGuard::new();
        let (serial_tool, concurrent_tool, serial_calls, concurrent_calls, replacement):
            GuardedCase = if name == "multi_edit" {
            (
                Arc::new(
                    MultiEditTool::new(Workspace::new(serial_dir.clone()))
                        .guard(serial_guard.clone()),
                ),
                Arc::new(
                    MultiEditTool::new(Workspace::new(concurrent_dir.clone()))
                        .guard(concurrent_guard.clone()),
                ),
                multi_calls("serial"),
                multi_calls("concurrent"),
                "EDITED",
            )
        } else {
            (
                Arc::new(
                    ApplyPatchTool::new(Workspace::new(serial_dir.clone()))
                        .guard(serial_guard.clone()),
                ),
                Arc::new(
                    ApplyPatchTool::new(Workspace::new(concurrent_dir.clone()))
                        .guard(concurrent_guard.clone()),
                ),
                patch_calls("serial"),
                patch_calls("concurrent"),
                "PATCHED",
            )
        };

        let (serial_results, serial) = dispatch(serial_tool, serial_calls, 1).await;
        let (concurrent_results, concurrent) =
            dispatch(concurrent_tool, concurrent_calls, CALLS).await;
        assert_results(&serial_results);
        assert_results(&concurrent_results);
        assert_markers(&serial_dir, "serial", replacement);
        assert_markers(&concurrent_dir, "concurrent", replacement);
        assert_eq!(serial_guard.len(), CALLS);
        assert_eq!(concurrent_guard.len(), CALLS);
        report_parallel(name, "guarded_distinct", serial, concurrent);
        std::fs::remove_dir_all(serial_dir).ok();
        std::fs::remove_dir_all(concurrent_dir).ok();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn one_call_mutates_many_files() {
    for name in ["multi_edit", "apply_patch"] {
        let dir = temp_dir(&format!("{name}-batch-files"));
        prepare(&dir, "batch");
        let (tool, batch_call, replacement): (Arc<dyn Tool>, ToolCall, &str) =
            if name == "multi_edit" {
                (
                    Arc::new(MultiEditTool::new(Workspace::new(dir.clone()))),
                    multi_batch_call("batch"),
                    "EDITED",
                )
            } else {
                (
                    Arc::new(ApplyPatchTool::new(Workspace::new(dir.clone()))),
                    patch_batch_call("batch"),
                    "PATCHED",
                )
            };
        let (results, wall) = dispatch(tool, vec![batch_call], 1).await;
        assert_single_result(&results);
        assert_markers(&dir, "batch", replacement);
        report_single(name, "batch_files", CALLS, wall);
        std::fs::remove_dir_all(dir).ok();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn one_file_receives_many_ordered_operations() {
    for name in ["multi_edit", "apply_patch"] {
        let dir = temp_dir(&format!("{name}-repeated"));
        prepare_repeated(&dir, "repeated.txt");
        let (tool, repeated_call, replacement): (Arc<dyn Tool>, ToolCall, &str) =
            if name == "multi_edit" {
                (
                    Arc::new(MultiEditTool::new(Workspace::new(dir.clone()))),
                    multi_repeated_call("repeated.txt"),
                    "EDITED",
                )
            } else {
                (
                    Arc::new(ApplyPatchTool::new(Workspace::new(dir.clone()))),
                    patch_repeated_call("repeated.txt"),
                    "PATCHED",
                )
            };
        let (results, wall) = dispatch(tool, vec![repeated_call], 1).await;
        assert_single_result(&results);
        assert_repeated_markers(&dir, "repeated.txt", replacement);
        report_single(name, "repeated_file", 1, wall);
        std::fs::remove_dir_all(dir).ok();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn one_call_appends_to_many_files() {
    for name in ["multi_edit", "apply_patch"] {
        let dir = temp_dir(&format!("{name}-append-files"));
        prepare(&dir, "append");
        let (tool, append_call): (Arc<dyn Tool>, ToolCall) = if name == "multi_edit" {
            (
                Arc::new(MultiEditTool::new(Workspace::new(dir.clone()))),
                multi_append_batch_call("append"),
            )
        } else {
            (
                Arc::new(ApplyPatchTool::new(Workspace::new(dir.clone()))),
                patch_append_batch_call("append"),
            )
        };
        let (results, wall) = dispatch(tool, vec![append_call], 1).await;
        assert_single_result(&results);
        assert_appended(&dir, "append");
        report_single(name, "append_files", CALLS, wall);
        std::fs::remove_dir_all(dir).ok();
    }
}
