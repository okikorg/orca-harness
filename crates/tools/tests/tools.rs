//! Tools exercised end-to-end through the Agent with a scripted model,
//! against a real temp workspace and the real host shell.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::time::timeout;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{Agent, CancellationToken, Message, ModelResponse, Tool, ToolContext};
use orca_harness_tools::{
    core_tools, EditFileTool, GrepTool, ListDirTool, ReadFileTool, ShellTool, Workspace,
    WriteFileTool,
};

const RUN_TIMEOUT: Duration = Duration::from_secs(20);

static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn temp_ws() -> (Workspace, std::path::PathBuf) {
    // Unique dir under the system temp without external crates.
    let n = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("orca-harness-tools-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    (Workspace::new(dir.clone()), dir)
}

fn ctx() -> ToolContext {
    ToolContext {
        call_id: "t".into(),
        tool_name: "t".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

#[tokio::test]
async fn write_then_read_roundtrips() {
    let (ws, dir) = temp_ws();
    let write = WriteFileTool::new(ws.clone());
    let read = ReadFileTool::new(ws.clone());

    let out = write
        .call(
            json!({"path": "sub/hello.txt", "content": "hi there"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["bytesWritten"], json!(8));
    assert!(dir.join("sub/hello.txt").exists());

    let got = read
        .call(json!({"path": "sub/hello.txt"}), &ctx())
        .await
        .unwrap();
    assert_eq!(got["content"], json!("hi there"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_requires_unique_match_unless_replace_all() {
    let (ws, dir) = temp_ws();
    let write = WriteFileTool::new(ws.clone());
    let edit = EditFileTool::new(ws.clone());
    write
        .call(json!({"path": "f.txt", "content": "a a a"}), &ctx())
        .await
        .unwrap();

    // Ambiguous single replace is rejected.
    let err = edit
        .call(json!({"path": "f.txt", "old": "a", "new": "b"}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("occurs 3 times"));

    // replaceAll succeeds.
    let out = edit
        .call(
            json!({"path": "f.txt", "old": "a", "new": "b", "replaceAll": true}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["replacements"], json!(3));
    assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "b b b");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn path_escape_is_rejected() {
    let (ws, dir) = temp_ws();
    let read = ReadFileTool::new(ws.clone());
    let err = read
        .call(json!({"path": "../../etc/passwd"}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("escapes"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn grep_finds_matches_and_lists_dir() {
    let (ws, dir) = temp_ws();
    let write = WriteFileTool::new(ws.clone());
    write
        .call(
            json!({"path": "a.txt", "content": "alpha\nneedle here\nbeta"}),
            &ctx(),
        )
        .await
        .unwrap();
    write
        .call(json!({"path": "b.txt", "content": "no match"}), &ctx())
        .await
        .unwrap();

    let grep = GrepTool::new(ws.clone());
    let out = grep.call(json!({"query": "needle"}), &ctx()).await.unwrap();
    let matches = out["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["line"], json!(2));
    assert_eq!(matches[0]["path"], json!("a.txt"));

    let list = ListDirTool::new(ws.clone());
    let entries = list.call(json!({"path": "."}), &ctx()).await.unwrap();
    let names: Vec<_> = entries["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["a.txt", "b.txt"]);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn shell_runs_a_command_on_the_host() {
    let shell = ShellTool::local();
    let out = shell
        .call(json!({"command": "echo hello && exit 0"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["stdout"], json!("hello\n"));
    assert_eq!(out["exitCode"], json!(0));
    assert_eq!(out["success"], json!(true));
}

#[tokio::test]
async fn shell_reports_nonzero_exit_without_erroring() {
    let shell = ShellTool::local();
    let out = shell
        .call(json!({"command": "echo oops >&2; exit 3"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["exitCode"], json!(3));
    assert_eq!(out["success"], json!(false));
    assert_eq!(out["stderr"], json!("oops\n"));
}

#[tokio::test]
async fn shell_times_out() {
    let shell = ShellTool::local().timeout(Some(Duration::from_millis(150)));
    let err = shell
        .call(json!({"command": "sleep 5"}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"));
}

#[tokio::test]
async fn shell_cancellation_kills_the_child() {
    let shell = ShellTool::local();
    let cancel = CancellationToken::new();
    let ctx = ToolContext {
        call_id: "t".into(),
        tool_name: "shell".into(),
        cancellation: cancel.clone(),
        deadline: None,
    };
    let handle =
        tokio::spawn(async move { shell.call(json!({"command": "sleep 30"}), &ctx).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = std::time::Instant::now();
    cancel.cancel();
    let result = timeout(Duration::from_secs(3), handle)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cancel must not wait for sleep"
    );
}

/// The whole point: a model drives the tools end-to-end and gets results
/// back. Model asks to write a file, then read it, then finishes.
#[tokio::test]
async fn agent_drives_core_tools_end_to_end() {
    let (ws, dir) = temp_ws();
    let model = ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call(
            "c0",
            "write_file",
            json!({"path": "note.md", "content": "# hi"}),
        )]),
        ModelResponse::tool_calls(vec![call("c1", "read_file", json!({"path": "note.md"}))]),
        ModelResponse::final_text("done"),
    ]);

    let mut agent = Agent::new(model);
    for tool in core_tools(&ws) {
        agent = agent.tool_arc(tool);
    }
    let answer = timeout(RUN_TIMEOUT, agent.run("write and read a note"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer, "done");
    assert_eq!(
        std::fs::read_to_string(dir.join("note.md")).unwrap(),
        "# hi"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Concurrent writes to the SAME path serialize (keyed), so no corruption;
/// this also exercises the tools through real dispatch.
#[tokio::test(flavor = "multi_thread")]
async fn same_path_writes_serialize() {
    let (ws, dir) = temp_ws();
    let calls: Vec<_> = (0..5)
        .map(|i| {
            call(
                &format!("c{i}"),
                "write_file",
                json!({"path": "shared.txt", "content": format!("v{i}")}),
            )
        })
        .collect();
    let model = ScriptedModel::tool_round(calls, "done");
    let mut agent = Agent::new(model);
    for tool in core_tools(&ws) {
        agent = agent.tool_arc(tool);
    }
    timeout(RUN_TIMEOUT, agent.run("write concurrently"))
        .await
        .unwrap()
        .unwrap();
    // File exists and holds one of the values intact (no interleaving).
    let content = std::fs::read_to_string(dir.join("shared.txt")).unwrap();
    assert!(
        ["v0", "v1", "v2", "v3", "v4"].contains(&content.as_str()),
        "corrupted content: {content:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 100 real `write_file` calls to DISTINCT paths, fanned out in one model
/// turn. Every file must land with the exact content its call carried —
/// this is real filesystem work under real concurrent dispatch, not a
/// scripted echo. Verifies both correctness and that the run completes in
/// wall-clock far under the serial sum.
#[tokio::test(flavor = "multi_thread")]
async fn hundred_concurrent_file_writes_all_land() {
    const N: usize = 100;
    let (ws, dir) = temp_ws();
    let calls: Vec<_> = (0..N)
        .map(|i| {
            call(
                &format!("c{i}"),
                "write_file",
                json!({"path": format!("out/f{i}.txt"), "content": format!("content-{i}")}),
            )
        })
        .collect();
    let model = ScriptedModel::tool_round(calls, "done");
    let mut agent = Agent::new(model).limits(orca_harness_core::Limits {
        max_parallel_tools: N,
        ..Default::default()
    });
    for tool in core_tools(&ws) {
        agent = agent.tool_arc(tool);
    }

    let begun = std::time::Instant::now();
    timeout(RUN_TIMEOUT, agent.run("write 100 files"))
        .await
        .unwrap()
        .unwrap();
    let elapsed = begun.elapsed();

    // Every file exists with exactly its own content.
    for i in 0..N {
        let got = std::fs::read_to_string(dir.join(format!("out/f{i}.txt"))).unwrap();
        assert_eq!(got, format!("content-{i}"), "file {i} wrong/missing");
    }
    // Concurrent I/O should finish well under a naive serial bound.
    assert!(
        elapsed < Duration::from_secs(5),
        "100 writes took {elapsed:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 32 real `shell` calls (each sleeps, then echoes its id) fanned out in
/// one turn. If dispatch were serial the sleeps would sum to ~1.6s; a
/// concurrent dispatch finishes in roughly one sleep. Also checks each
/// call's stdout is correctly paired back to its own result.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_shell_calls_run_in_parallel() {
    const N: usize = 32;
    let calls: Vec<_> = (0..N)
        .map(|i| {
            call(
                &format!("c{i}"),
                "shell",
                json!({"command": format!("sleep 0.05; echo id-{i}")}),
            )
        })
        .collect();
    let model = Arc::new(ScriptedModel::tool_round(calls, "done"));
    let shell = ShellTool::local();
    let agent = Agent::new(model.clone())
        .tool(shell)
        .limits(orca_harness_core::Limits {
            max_parallel_tools: N,
            ..Default::default()
        });

    let begun = std::time::Instant::now();
    timeout(RUN_TIMEOUT, agent.run("fan out shells"))
        .await
        .unwrap()
        .unwrap();
    let elapsed = begun.elapsed();

    // 32 × 50ms serial = 1.6s; concurrent should be a small multiple of one.
    assert!(
        elapsed < Duration::from_millis(800),
        "32 concurrent 50ms shells took {elapsed:?} — not parallel"
    );

    // Each result carries its own echoed id, in original call order.
    let results = model
        .observed_contexts()
        .last()
        .unwrap()
        .messages()
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Tool { results } => Some(results.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(results.len(), N);
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r.call_id, format!("c{i}"));
        assert_eq!(r.output["stdout"], json!(format!("id-{i}\n")));
        assert_eq!(r.output["success"], json!(true));
    }
}

/// Silence unused-import warnings for the transcript helper on Message.
#[allow(dead_code)]
fn _touch(m: &Message) -> bool {
    matches!(m, Message::Tool { .. })
}
