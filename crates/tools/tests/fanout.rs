//! One batch, every tool: proves the whole shipped tool set fans out
//! concurrently through the real Dispatcher, and that batch wall clock
//! tracks the slowest member rather than the sum.

use std::time::{Duration, Instant};

use serde_json::json;

use orca_harness_core::testing::call;
use orca_harness_core::{CancellationToken, Dispatcher, ExtensionRegistry, ToolRegistry};
use orca_harness_tools::{core_tools, fs_admin_tools, Workspace};

static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn temp_ws() -> (Workspace, std::path::PathBuf) {
    let n = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("orca-harness-fanout-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    (Workspace::new(dir.clone()), dir)
}

#[tokio::test(flavor = "multi_thread")]
async fn every_tool_fans_out_in_one_batch() {
    let (ws, dir) = temp_ws();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/seed.rs"), "needle in seed\n").unwrap();
    std::fs::write(dir.join("edit_me.txt"), "before\n").unwrap();
    std::fs::write(dir.join("multi_edit_me.txt"), "before\n").unwrap();
    std::fs::write(dir.join("patch_me.txt"), "before\n").unwrap();
    std::fs::write(dir.join("copy_me.txt"), "payload\n").unwrap();
    std::fs::write(dir.join("move_me.txt"), "mv\n").unwrap();
    std::fs::write(dir.join("delete_me.txt"), "rm\n").unwrap();

    let mut tools = ToolRegistry::new();
    for tool in core_tools(&ws).into_iter().chain(fs_admin_tools(&ws)) {
        tools.register(tool);
    }

    // Three slow members (two 400ms shells, one 300ms process spawn) plus
    // one call to every other tool. Serial lower bound for the slow trio
    // alone is 1.1s; a parallel batch should finish near the slowest one.
    let batch = vec![
        call("c-shell-1", "shell", json!({"command": "sleep 0.4"})),
        call("c-shell-2", "shell", json!({"command": "sleep 0.4"})),
        call(
            "c-proc",
            "process",
            json!({"action": "spawn", "command": "sleep 0.3"}),
        ),
        call("c-read", "read_file", json!({"path": "src/seed.rs"})),
        call(
            "c-write",
            "write_file",
            json!({"path": "fresh.txt", "content": "hi"}),
        ),
        call(
            "c-edit",
            "edit_file",
            json!({"path": "edit_me.txt", "old": "before", "new": "after"}),
        ),
        call(
            "c-multi-edit",
            "multi_edit",
            json!({"edits": [{"path": "multi_edit_me.txt", "old": "before", "new": "after"}]}),
        ),
        call(
            "c-apply-patch",
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Update File: patch_me.txt\n@@\n-before\n+after\n*** End Patch"}),
        ),
        call("c-list", "list_dir", json!({"path": "."})),
        call("c-grep", "grep", json!({"query": "needle"})),
        call("c-glob", "glob", json!({"pattern": "*.rs"})),
        call(
            "c-copy",
            "copy_file",
            json!({"from": "copy_me.txt", "to": "copied.txt"}),
        ),
        call(
            "c-rename",
            "rename_file",
            json!({"from": "move_me.txt", "to": "moved.txt"}),
        ),
        call("c-delete", "delete_file", json!({"path": "delete_me.txt"})),
        call("c-mkdir", "create_folder", json!({"path": "made/dir"})),
        call("c-info", "file_info", json!({"path": "copy_me.txt"})),
    ];
    let n = batch.len();

    let started = Instant::now();
    let results = Dispatcher::new()
        .execute(
            batch,
            &tools,
            &ExtensionRegistry::new(),
            &CancellationToken::new(),
            None,
            n,
        )
        .await
        .unwrap();
    let wall = started.elapsed();

    assert_eq!(results.len(), n);
    for result in &results {
        assert!(
            !result.is_error,
            "{} ({}) failed: {}",
            result.tool_name, result.call_id, result.output
        );
    }
    assert!(
        wall < Duration::from_millis(900),
        "batch must track the slowest member (~400ms), not the 1.1s+ serial sum; took {wall:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
