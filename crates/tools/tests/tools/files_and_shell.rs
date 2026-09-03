/// A rename mutates exactly two paths: it must key on both (serializing
/// against writes to either path) instead of excluding the whole batch.
#[test]
fn rename_keys_on_both_paths_not_globally_exclusive() {
    let (ws, dir) = temp_ws();
    let rename = RenameFileTool::new(ws);
    assert_eq!(
        rename.concurrency(&json!({"from": "a.txt", "to": "b/c.txt"})),
        orca_harness_core::Concurrency::Keys(vec!["file:a.txt".into(), "file:b/c.txt".into()])
    );
    // Malformed input still falls back to exclusive.
    assert_eq!(
        rename.concurrency(&json!({"from": "a.txt"})),
        orca_harness_core::Concurrency::Serial
    );
    std::fs::remove_dir_all(&dir).ok();
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

/// A model that guesses a path (`tests/mod.rs` in a crate that wires tests
/// with `include!`) should be redirected, not just refused: the error names
/// the nearest directory that does exist and what it holds.
#[tokio::test]
async fn read_of_a_missing_file_lists_the_nearest_existing_directory() {
    let (ws, dir) = temp_ws();
    std::fs::create_dir_all(dir.join("tui/tests/inner")).unwrap();
    std::fs::write(dir.join("tui/tests/core_2.rs"), "").unwrap();
    std::fs::write(dir.join("tui/tests/paste.rs"), "").unwrap();
    std::fs::write(dir.join("tui/inner_tests.rs"), "").unwrap();
    let read = ReadFileTool::new(ws.clone());

    // Parent exists: list it.
    let err = read
        .call(json!({"path": "tui/tests/mod.rs"}), &ctx())
        .await
        .unwrap_err();
    assert!(err.message.starts_with("read failed: "), "{}", err.message);
    assert!(
        err.message
            .contains("`tui/tests` contains: core_2.rs, inner/, paste.rs"),
        "{}",
        err.message
    );

    // Parent is missing too: climb to the nearest ancestor that exists.
    let err = read
        .call(json!({"path": "tui/nope/deeper/x.rs"}), &ctx())
        .await
        .unwrap_err();
    assert!(
        err.message.contains("`tui` contains: inner_tests.rs, tests/"),
        "{}",
        err.message
    );

    // Nothing but the root exists: say so in workspace-relative terms.
    let err = read
        .call(json!({"path": "package.json"}), &ctx())
        .await
        .unwrap_err();
    assert!(
        err.message.contains("workspace root contains: tui/"),
        "{}",
        err.message
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_requires_unique_match_unless_replace_all() {
    let (ws, dir) = temp_ws();
    let write = WriteFileTool::new(ws.clone());
    let edit = EditFileTool::new(ws.clone());
    write
        .call(json!({"path": "f.txt", "content": "a\na\n\na"}), &ctx())
        .await
        .unwrap();

    // Ambiguous single replace is rejected, and the error says where each
    // occurrence is so the retry can add the right context.
    let err = edit
        .call(json!({"path": "f.txt", "old": "a", "new": "b"}), &ctx())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("occurs 3 times (lines 1, 2, 4)"),
        "{err}"
    );

    // replaceAll succeeds.
    let out = edit
        .call(
            json!({"path": "f.txt", "old": "a", "new": "b", "replaceAll": true}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["replacements"], json!(3));
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "b\nb\n\nb"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn multi_edit_ambiguity_names_the_occurrence_lines() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("f.rs"), "fn a() {\n    x\n}\nfn b() {\n    x\n}\n").unwrap();
    let err = MultiEditTool::new(ws)
        .call(
            json!({"edits": [{"path": "f.rs", "old": "    x\n", "new": "    y\n"}]}),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("`old` occurs 2 times (lines 2, 5)"),
        "{err}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Read-before-write, the whole contract in one test: a new file needs
/// no read, an existing one does, reading it unblocks the overwrite, and
/// the tool's own write keeps it unblocked.
#[tokio::test]
async fn guarded_write_needs_a_prior_read_of_an_existing_file() {
    let (ws, dir) = temp_ws();
    let guard = FileGuard::new();
    let write = WriteFileTool::new(ws.clone()).guard(guard.clone());
    let read = ReadFileTool::new(ws.clone()).guard(guard.clone());

    // Creating a file is not an overwrite: nothing to have read.
    write
        .call(json!({"path": "new.txt", "content": "one"}), &ctx())
        .await
        .unwrap();
    // ...and having written it, this tool may write it again.
    write
        .call(json!({"path": "new.txt", "content": "two"}), &ctx())
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("new.txt")).unwrap(), "two");

    // A file that appeared from outside must be read first.
    std::fs::write(dir.join("theirs.txt"), "precious").unwrap();
    let err = write
        .call(
            json!({"path": "theirs.txt", "content": "clobbered"}),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("has not been read"), "{err}");
    assert_eq!(
        std::fs::read_to_string(dir.join("theirs.txt")).unwrap(),
        "precious",
        "a refused write must not have happened"
    );

    // Reading it is what makes the overwrite deliberate.
    read.call(json!({"path": "theirs.txt"}), &ctx())
        .await
        .unwrap();
    write
        .call(
            json!({"path": "theirs.txt", "content": "on purpose"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("theirs.txt")).unwrap(),
        "on purpose"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The case the guard exists for: something else edits the file between
/// the read and the write.
#[tokio::test]
async fn guarded_write_refuses_a_file_that_changed_after_the_read() {
    let (ws, dir) = temp_ws();
    let guard = FileGuard::new();
    let write = WriteFileTool::new(ws.clone()).guard(guard.clone());
    let read = ReadFileTool::new(ws.clone()).guard(guard.clone());

    std::fs::write(dir.join("f.txt"), "original").unwrap();
    read.call(json!({"path": "f.txt"}), &ctx()).await.unwrap();

    // Somebody else saves the file (a longer body, so length alone
    // proves the change whatever the filesystem's mtime resolution is).
    std::fs::write(dir.join("f.txt"), "edited by someone else").unwrap();

    let err = write
        .call(
            json!({"path": "f.txt", "content": "from stale context"}),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("changed on disk"), "{err}");
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "edited by someone else"
    );

    // Re-reading adopts the new state and clears the way.
    read.call(json!({"path": "f.txt"}), &ctx()).await.unwrap();
    write
        .call(json!({"path": "f.txt", "content": "now informed"}), &ctx())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "now informed"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `edit_file` is exempt from the check — it works from the contents it
/// just read — but it must still stamp what it wrote, or the next
/// `write_file` to that path would see a file that "changed on disk".
#[tokio::test]
async fn edit_is_exempt_from_the_check_and_still_stamps() {
    let (ws, dir) = temp_ws();
    let guard = FileGuard::new();
    let write = WriteFileTool::new(ws.clone()).guard(guard.clone());
    let edit = EditFileTool::new(ws.clone()).guard(guard.clone());

    std::fs::write(dir.join("f.txt"), "before").unwrap();
    // Never read through the guard, and the edit goes through anyway.
    edit.call(
        json!({"path": "f.txt", "old": "before", "new": "after"}),
        &ctx(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "after");

    // The edit's own write left a stamp, so the overwrite is allowed.
    write
        .call(json!({"path": "f.txt", "content": "replaced"}), &ctx())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "replaced"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Clearing the guard is how a host says "that conversation is over":
/// the next overwrite has to read the file again.
#[tokio::test]
async fn clearing_the_guard_requires_reading_again() {
    let (ws, dir) = temp_ws();
    let guard = FileGuard::new();
    let write = WriteFileTool::new(ws.clone()).guard(guard.clone());

    write
        .call(json!({"path": "f.txt", "content": "one"}), &ctx())
        .await
        .unwrap();
    assert_eq!(guard.len(), 1);
    guard.clear();
    assert!(guard.is_empty());

    let err = write
        .call(json!({"path": "f.txt", "content": "two"}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("has not been read"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Unguarded tools are unchanged: the guard is opt-in, and library users
/// who build the tools themselves get the old behavior.
#[tokio::test]
async fn an_unguarded_write_overwrites_anything() {
    let (ws, dir) = temp_ws();
    let write = WriteFileTool::new(ws.clone());
    std::fs::write(dir.join("f.txt"), "theirs").unwrap();
    write
        .call(json!({"path": "f.txt", "content": "mine"}), &ctx())
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "mine");
    std::fs::remove_dir_all(&dir).ok();
}

/// `core_tools` wires the guard by default, so the shipped set has
/// read-before-write without the host asking for it.
#[tokio::test]
async fn core_tools_ship_with_the_guard_wired() {
    let (ws, dir) = temp_ws();
    std::fs::write(dir.join("f.txt"), "theirs").unwrap();
    let tools = core_tools(&ws);
    let write = tools
        .iter()
        .find(|tool| tool.schema().name == "write_file")
        .expect("write_file in core_tools");
    let err = write
        .call(json!({"path": "f.txt", "content": "mine"}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("has not been read"), "{err}");
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "theirs"
    );
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

/// `sh -c ""` exits 0 doing nothing; accepting it would report success
/// for a command that never existed.
#[tokio::test]
async fn shell_rejects_an_empty_command() {
    let shell = ShellTool::local();
    let err = shell
        .call(json!({"command": "   "}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("empty"), "got: {err}");
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
