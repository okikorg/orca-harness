// ---- process host controller ---------------------------------------------
//
// The typed `ProcessController` and the JSON `process` tool run the same
// implementation over the same manager: ids, state, output, and errors
// must agree whichever path a caller takes.

fn entry_ids(entries: &[ProcessEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.id.clone()).collect()
}

#[tokio::test]
async fn typed_spawn_and_tool_poll_share_ids_and_state() {
    let tool = ProcessTool::local();
    let controller = tool.controller();
    assert!(controller.is_open());

    let spawned = controller
        .spawn(
            ProcessSpawn::new("printf ready; sleep 2"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(spawned.running);
    assert_eq!(spawned.output, "ready");
    assert_eq!(spawned.exit_code, None);
    let id = spawned.id.clone();

    let via_tool = tool
        .call(json!({"action": "poll", "id": id, "waitMs": 0}), &ctx())
        .await
        .unwrap();
    assert_eq!(via_tool["id"], json!(id));
    assert_eq!(via_tool["running"], json!(true));
    assert_eq!(via_tool["output"], json!(""));

    let via_controller = controller
        .poll(&id, Some(Duration::ZERO), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(via_controller.id, id);
    assert!(via_controller.running);
    assert_eq!(via_controller.output, "");

    let listed = tool.call(json!({"action": "list"}), &ctx()).await.unwrap();
    assert_eq!(listed["processes"][0]["id"], json!(id));
    assert_eq!(listed["processes"][0]["running"], json!(true));
    let entries = controller.list().unwrap();
    assert_eq!(entry_ids(&entries), vec![id.clone()]);
    assert_eq!(entries[0].command, "printf ready; sleep 2");
    assert!(entries[0].running);

    controller.kill(&id).await.unwrap();
    assert!(controller.list().unwrap().is_empty());
}

#[tokio::test]
async fn typed_write_eof_and_kill_match_the_tool() {
    let tool = ProcessTool::local();
    let controller = tool.controller();
    let id = controller
        .spawn(ProcessSpawn::new("cat"), CancellationToken::new())
        .await
        .unwrap()
        .id;

    let echoed = controller
        .write(&id, ProcessWrite::new("hello"), CancellationToken::new())
        .await
        .unwrap();
    assert!(echoed.running);
    assert_eq!(echoed.output, "hello\n");

    let closed = controller
        .write(&id, ProcessWrite::new("").newline(false).eof(true), CancellationToken::new())
        .await
        .unwrap();
    assert!(!closed.running, "cat exits once stdin reaches EOF");
    assert_eq!(closed.exit_code, Some(0));

    let err = controller
        .write(&id, ProcessWrite::new("more"), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), format!("process {id} has exited"));

    // Kill reaps the exited entry; a second kill on either path reports
    // the same unknown-id error.
    let killed = controller.kill(&id).await.unwrap();
    assert_eq!(killed.exit_code, Some(0));
    let typed_err = controller.kill(&id).await.unwrap_err();
    let tool_err = tool
        .call(json!({"action": "kill", "id": id}), &ctx())
        .await
        .unwrap_err();
    assert_eq!(typed_err.to_string(), format!("unknown process id: {id}"));
    assert_eq!(typed_err.to_string(), tool_err.to_string());
}

#[tokio::test]
async fn typed_output_is_bounded() {
    let tool = ProcessTool::local().max_output_bytes(64);
    let controller = tool.controller();

    // 10 KB in one go: a single response returns at most 64 bytes.
    let snapshot = controller
        .spawn(
            ProcessSpawn::new("head -c 10240 /dev/zero | tr '\\0' x").wait_for_exit(true),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!snapshot.running);
    assert_eq!(snapshot.output.len(), 64);
    assert!(snapshot.more_output);
    assert_eq!(snapshot.dropped_bytes, 0);

    // Beyond the default 512 KB unread cap, the oldest bytes are dropped
    // and the drop is reported exactly once.
    let flooded = controller
        .spawn(
            ProcessSpawn::new("head -c 600000 /dev/zero | tr '\\0' y").wait_for_exit(true),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(flooded.output.len(), 64);
    assert!(flooded.more_output);
    assert!(flooded.dropped_bytes > 0, "{flooded:?}");
    let again = controller
        .poll(&flooded.id, Some(Duration::ZERO), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(again.dropped_bytes, 0, "a drop is reported once");
    assert_eq!(again.into_value().get("droppedBytes"), None);
}

#[tokio::test]
async fn failed_launch_cleans_up() {
    let stats = BackgroundStats::new();
    let tool = ProcessTool::new(Executor::new(
        "/nonexistent/orca-harness-bogus-program",
        Vec::<String>::new(),
    ))
    .stats(stats.clone());
    let controller = tool.controller();

    let err = controller
        .spawn(ProcessSpawn::new("true"), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("failed to spawn"), "got: {err}");
    assert_eq!(stats.processes(), 0);
    assert!(stats.process_list().is_empty());
    assert!(controller.list().unwrap().is_empty());
    let listed = tool.call(json!({"action": "list"}), &ctx()).await.unwrap();
    assert_eq!(listed, json!({"processes": []}));

    // Validation failures are the same on both paths and leak nothing.
    let err = controller
        .spawn(ProcessSpawn::new("  "), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "`command` must not be empty");
    let err = controller
        .spawn(
            ProcessSpawn::new("sleep 1").notify_on_match(""),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "`notifyOnMatch` must not be empty");
    assert_eq!(stats.processes(), 0);
}

#[tokio::test]
async fn controller_reports_closed_after_tool_drop() {
    let stats = BackgroundStats::new();
    let controller = {
        let tool = ProcessTool::local().stats(stats.clone());
        let controller = tool.controller();
        controller
            .spawn(
                ProcessSpawn::new("sleep 277.8 & wait"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(stats.processes(), 1);
        controller
    }; // tool dropped here
    tokio::time::sleep(Duration::from_millis(300)).await;
    let found = std::process::Command::new("pgrep")
        .args(["-f", "sleep 277.8"])
        .output()
        .unwrap();
    assert!(
        !found.status.success(),
        "children must die when the tool is dropped, controller or not"
    );
    assert_eq!(stats.processes(), 0);
    assert!(!controller.is_open());
    let err = controller.list().unwrap_err();
    assert_eq!(err.to_string(), "process manager is closed");
    let err = controller
        .spawn(ProcessSpawn::new("true"), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "process manager is closed");
}

#[tokio::test]
async fn spawn_cancellation_leaves_process_running_via_controller() {
    let tool = ProcessTool::local();
    let controller = tool.controller();
    let cancellation = CancellationToken::new();
    let trigger = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });

    let err = controller
        .spawn(
            ProcessSpawn::new("sleep 10").wait_for_exit(true),
            cancellation,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cancelled"));

    let entries = controller.list().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].running);
    let listed = tool.call(json!({"action": "list"}), &ctx()).await.unwrap();
    assert_eq!(listed["processes"][0]["id"], json!(entries[0].id));
    let killed = controller.kill(&entries[0].id).await.unwrap();
    assert!(!killed.running);
}

#[tokio::test]
async fn tool_json_output_unchanged() {
    let tool = ProcessTool::local();
    let controller = tool.controller();

    let spawned = controller
        .spawn(
            ProcessSpawn::new("printf one; exit 3").wait_for_exit(true),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let id = spawned.id.clone();
    assert_eq!(
        spawned.into_value(),
        json!({
            "id": id,
            "output": "one",
            "running": false,
            "exitCode": 3,
            "moreOutput": false,
        })
    );

    // Once the process is quiet, both paths drain the same (empty)
    // buffer and must describe the same instant identically.
    let via_tool = tool
        .call(json!({"action": "poll", "id": id}), &ctx())
        .await
        .unwrap();
    let via_controller = controller
        .poll(&id, None, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(via_controller.into_value(), via_tool);
    assert_eq!(via_tool["exitCode"], json!(3));

    let listed = tool.call(json!({"action": "list"}), &ctx()).await.unwrap();
    assert_eq!(ProcessEntry::list_into_value(controller.list().unwrap()), listed);
    assert_eq!(
        listed,
        json!({"processes": [{
            "id": id,
            "command": "printf one; exit 3",
            "running": false,
            "exitCode": 3,
        }]})
    );

    let killed = tool
        .call(json!({"action": "kill", "id": id}), &ctx())
        .await
        .unwrap();
    assert_eq!(killed["exitCode"], json!(3));
}
