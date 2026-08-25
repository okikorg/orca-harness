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
    let bun = bun_available();
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
    // 4 file_info
    for i in 0..4 {
        batch.push(call(
            &id(),
            "file_info",
            json!({"path": format!("pool/large_{}.txt", i % POOL)}),
        ));
    }
    // 4 bun_repl (one independent Keyed chain), or file_info without Bun
    if bun {
        let code = "globalThis.bunTotal = (globalThis.bunTotal ?? 0) + 1; console.log(globalThis.bunTotal)";
        for _ in 0..4 {
            batch.push(call(&id(), "bun_repl", json!({"code": code})));
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
    if bun {
        let totals: Vec<&str> = results
            .iter()
            .filter(|r| r.tool_name == "bun_repl")
            .map(|r| r.output["output"].as_str().unwrap().trim())
            .collect();
        assert_eq!(totals, ["1", "2", "3", "4"]);
    }
    for r in results.iter().filter(|r| r.tool_name == "subagent") {
        assert_eq!(r.output["answer"], "done");
    }

    let rate = BURST as f64 / wall.as_secs_f64().max(f64::EPSILON);
    eprintln!(
        "[bench] heterogeneous 16-tool mix          burst{BURST}={wall:>10.2?}  rate={rate:>7.0}/s"
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
