fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn bun_available() -> bool {
    std::process::Command::new("bun")
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
