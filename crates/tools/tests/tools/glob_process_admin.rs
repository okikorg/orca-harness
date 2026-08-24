// ---- glob -----------------------------------------------------------------

#[tokio::test]
async fn glob_matches_bare_anchored_and_recursive_patterns() {
    let (ws, dir) = temp_ws();
    let write = WriteFileTool::new(ws.clone());
    for (path, content) in [
        ("src/a.rs", "x"),
        ("src/deep/b.rs", "x"),
        ("c.txt", "x"),
        ("src/note.md", "x"),
    ] {
        write
            .call(json!({"path": path, "content": content}), &ctx())
            .await
            .unwrap();
    }
    let glob = GlobTool::new(ws.clone());

    // Bare pattern searches the whole tree.
    let out = glob.call(json!({"pattern": "*.rs"}), &ctx()).await.unwrap();
    assert_eq!(
        out["matches"],
        json!(["src/a.rs", "src/deep/b.rs"]),
        "sorted, tree-wide"
    );

    // Anchored single-star stays at one level.
    let out = glob
        .call(json!({"pattern": "src/*.rs"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["matches"], json!(["src/a.rs"]));

    // `**` spans levels (including zero).
    let out = glob
        .call(json!({"pattern": "src/**/*.rs"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["matches"], json!(["src/a.rs", "src/deep/b.rs"]));
    assert_eq!(out["truncated"], json!(false));
    assert!(
        out.get("skippedDirs").is_none(),
        "a clean walk reports no skips"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// An unreadable directory must be counted and reported, never silently
/// dropped: the caller has to be able to tell "no match" from "could
/// not look".
#[cfg(unix)]
#[tokio::test]
async fn glob_counts_unreadable_directories_instead_of_dropping_them() {
    use std::os::unix::fs::PermissionsExt;

    let (ws, dir) = temp_ws();
    let write = WriteFileTool::new(ws.clone());
    for path in ["open/a.rs", "sealed/b.rs"] {
        write
            .call(json!({"path": path, "content": "x"}), &ctx())
            .await
            .unwrap();
    }
    let sealed = dir.join("sealed");
    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&sealed).is_ok() {
        // Running as root: permission bits don't bite, the scenario
        // cannot be built. Restore and bail.
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        return;
    }

    let glob = GlobTool::new(ws.clone());
    let out = glob.call(json!({"pattern": "*.rs"}), &ctx()).await.unwrap();
    assert_eq!(out["matches"], json!(["open/a.rs"]));
    assert_eq!(out["skippedDirs"], json!(1), "the sealed dir is reported");

    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// ---- process --------------------------------------------------------------

#[tokio::test]
async fn process_runs_a_quick_command_to_completion() {
    let tool = ProcessTool::local();
    let out = tool
        .call(json!({"action": "spawn", "command": "echo hi"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["output"], json!("hi\n"));
    assert_eq!(out["running"], json!(false));
    assert_eq!(out["exitCode"], json!(0));
}

#[tokio::test]
async fn process_polls_a_background_command_until_output_arrives() {
    let tool = ProcessTool::local();
    let out = tool
        .call(
            json!({"action": "spawn", "command": "sleep 1 && echo done"}),
            &ctx(),
        )
        .await
        .unwrap();
    let id = out["id"].as_str().unwrap().to_string();
    assert_eq!(out["running"], json!(true));

    let polled = tool
        .call(json!({"action": "poll", "id": id, "waitMs": 5000}), &ctx())
        .await
        .unwrap();
    assert!(
        polled["output"].as_str().unwrap().contains("done"),
        "poll should wake on output, got: {polled}"
    );
}

#[tokio::test]
async fn process_spawn_rejects_an_empty_command() {
    let tool = ProcessTool::local();
    let err = tool
        .call(json!({"action": "spawn", "command": ""}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("empty"), "got: {err}");
}

/// Once a poll reports `running: false` with no more buffered output,
/// the story must be complete: output may never trickle in afterwards.
/// Guards the exit-vs-drain window where a snapshot could pair "exited"
/// with output still in the pipes.
#[tokio::test]
async fn process_exit_reports_never_hide_trailing_output() {
    let tool = ProcessTool::local();
    // Outlives spawn's settle window so the exit lands mid-polling.
    let out = tool
        .call(
            json!({"action": "spawn", "command": "sleep 0.7; echo tail-marker"}),
            &ctx(),
        )
        .await
        .unwrap();
    let id = out["id"].as_str().unwrap().to_string();
    let mut collected = out["output"].as_str().unwrap().to_string();

    for _ in 0..100 {
        let polled = tool
            .call(json!({"action": "poll", "id": id, "waitMs": 200}), &ctx())
            .await
            .unwrap();
        collected.push_str(polled["output"].as_str().unwrap());
        if polled["running"] == json!(false) && polled["moreOutput"] == json!(false) {
            break;
        }
    }
    assert!(
        collected.contains("tail-marker"),
        "an exit report must include all output, got: {collected:?}"
    );
}

#[tokio::test]
async fn process_drives_an_interactive_child_and_kills_it() {
    let tool = ProcessTool::local();
    let out = tool
        .call(json!({"action": "spawn", "command": "cat"}), &ctx())
        .await
        .unwrap();
    let id = out["id"].as_str().unwrap().to_string();
    assert_eq!(out["running"], json!(true));

    let echoed = tool
        .call(
            json!({"action": "write", "id": id, "input": "hello"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(echoed["output"], json!("hello\n"));

    let listed = tool.call(json!({"action": "list"}), &ctx()).await.unwrap();
    assert_eq!(listed["processes"].as_array().unwrap().len(), 1);

    let killed = tool
        .call(json!({"action": "kill", "id": id}), &ctx())
        .await
        .unwrap();
    assert_eq!(killed["running"], json!(false));

    // The id is gone after kill.
    let err = tool
        .call(json!({"action": "poll", "id": id}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown process id"));
}

#[tokio::test]
async fn process_write_eof_lets_stdin_readers_finish() {
    let tool = ProcessTool::local();
    let out = tool
        .call(json!({"action": "spawn", "command": "wc -l"}), &ctx())
        .await
        .unwrap();
    let id = out["id"].as_str().unwrap().to_string();
    let out = tool
        .call(
            json!({"action": "write", "id": id, "input": "a\nb", "eof": true}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["running"], json!(false), "wc exits on EOF: {out}");
    assert!(out["output"].as_str().unwrap().trim().ends_with('2'));
}

// ---- fs admin -------------------------------------------------------------

#[tokio::test]
async fn fs_admin_roundtrip() {
    let (ws, dir) = temp_ws();
    let write = WriteFileTool::new(ws.clone());
    write
        .call(json!({"path": "a.txt", "content": "data"}), &ctx())
        .await
        .unwrap();

    let mkdir = CreateFolderTool::new(ws.clone());
    mkdir
        .call(json!({"path": "nested/dir"}), &ctx())
        .await
        .unwrap();
    assert!(dir.join("nested/dir").is_dir());

    let copy = CopyFileTool::new(ws.clone());
    let out = copy
        .call(json!({"from": "a.txt", "to": "nested/b.txt"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["bytesCopied"], json!(4));

    let rename = RenameFileTool::new(ws.clone());
    rename
        .call(json!({"from": "nested/b.txt", "to": "c.txt"}), &ctx())
        .await
        .unwrap();
    assert!(dir.join("c.txt").exists());
    assert!(!dir.join("nested/b.txt").exists());

    let info = FileInfoTool::new(ws.clone());
    let meta = info.call(json!({"path": "c.txt"}), &ctx()).await.unwrap();
    assert_eq!(meta["exists"], json!(true));
    assert_eq!(meta["kind"], json!("file"));
    assert_eq!(meta["sizeBytes"], json!(4));
    let missing = info.call(json!({"path": "nope"}), &ctx()).await.unwrap();
    assert_eq!(missing["exists"], json!(false));

    let del = DeleteFileTool::new(ws.clone());
    // Directory without recursive is refused.
    let err = del
        .call(json!({"path": "nested"}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("recursive"));
    del.call(json!({"path": "nested", "recursive": true}), &ctx())
        .await
        .unwrap();
    assert!(!dir.join("nested").exists());
    del.call(json!({"path": "c.txt"}), &ctx()).await.unwrap();
    assert!(!dir.join("c.txt").exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[tokio::test]
async fn shell_timeout_kills_grandchildren() {
    let tool = ShellTool::local().timeout(Some(Duration::from_millis(400)));
    let err = tool
        .call(json!({"command": "sleep 281.7 & wait"}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let found = std::process::Command::new("pgrep")
        .args(["-f", "sleep 281.7"])
        .output()
        .unwrap();
    assert!(
        !found.status.success(),
        "grandchild sleep must die with the timed-out shell call"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn process_kill_reaches_grandchildren() {
    let tool = ProcessTool::local();
    let out = tool
        .call(
            json!({"action": "spawn", "command": "sleep 279.3 & wait"}),
            &ctx(),
        )
        .await
        .unwrap();
    let id = out["id"].as_str().unwrap().to_string();
    let found = std::process::Command::new("pgrep")
        .args(["-f", "sleep 279.3"])
        .output()
        .unwrap();
    assert!(found.status.success(), "grandchild should be running");

    tool.call(json!({"action": "kill", "id": id}), &ctx())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let found = std::process::Command::new("pgrep")
        .args(["-f", "sleep 279.3"])
        .output()
        .unwrap();
    assert!(
        !found.status.success(),
        "grandchild must die with the kill action"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_process_tool_kills_children_synchronously() {
    {
        let tool = ProcessTool::local();
        tool.call(
            json!({"action": "spawn", "command": "sleep 277.9 & wait"}),
            &ctx(),
        )
        .await
        .unwrap();
    } // tool dropped here
    tokio::time::sleep(Duration::from_millis(300)).await;
    let found = std::process::Command::new("pgrep")
        .args(["-f", "sleep 277.9"])
        .output()
        .unwrap();
    assert!(
        !found.status.success(),
        "children must die when the tool is dropped"
    );
}

#[tokio::test]
async fn process_stats_track_live_children() {
    let stats = BackgroundStats::new();
    let tool = ProcessTool::local().stats(stats.clone());
    assert_eq!(stats.processes(), 0);
    let out = tool
        .call(json!({"action": "spawn", "command": "sleep 259.1"}), &ctx())
        .await
        .unwrap();
    assert_eq!(stats.processes(), 1);
    let id = out["id"].as_str().unwrap().to_string();
    tool.call(json!({"action": "kill", "id": id}), &ctx())
        .await
        .unwrap();
    assert_eq!(stats.processes(), 0);
}

#[tokio::test]
async fn process_stats_zero_after_tool_drop() {
    let stats = BackgroundStats::new();
    {
        let tool = ProcessTool::local().stats(stats.clone());
        tool.call(json!({"action": "spawn", "command": "sleep 258.3"}), &ctx())
            .await
            .unwrap();
        assert_eq!(stats.processes(), 1);
    }
    assert_eq!(stats.processes(), 0);
}
