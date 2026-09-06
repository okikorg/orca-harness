#[test]
fn parallel_batch_results_are_labeled_with_their_tool() {
    let mut app = test_app();
    for (id, name, args) in [
        ("c1", "list_dir", serde_json::json!({"path": "."})),
        ("c2", "shell", serde_json::json!({"command": "git log"})),
    ] {
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: id.into(),
                tool_name: name.into(),
                input: args,
            },
            80,
        );
    }
    // Results complete out of call order.
    handle_harness_event(
        &mut app,
        HarnessEvent::ToolResult {
            tool_call_id: "c1".into(),
            tool_name: "list_dir".into(),
            output: serde_json::json!({"path": ".", "entries": ["a", "b"]}),
            is_error: false,
        },
        80,
    );
    handle_harness_event(
        &mut app,
        HarnessEvent::ToolResult {
            tool_call_id: "c2".into(),
            tool_name: "shell".into(),
            output: serde_json::json!({"stdout": "abc123", "exitCode": 0}),
            is_error: false,
        },
        80,
    );
    let joined = activity_lines(&app, 80, true)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let list_call = joined.find("List   .").unwrap();
    let list_result = joined.find("· 2 entries").unwrap();
    let shell_call = joined.find("Run    $ git log").unwrap();
    let shell_result = joined.find("· exit 0 · abc123").unwrap();
    assert!(
        list_call < list_result && shell_call < shell_result,
        "results stay attached: {joined}"
    );

    // A later lone call joins the same work group without losing the
    // association between any earlier call and result.
    handle_harness_event(
        &mut app,
        HarnessEvent::ToolCall {
            tool_call_id: "c3".into(),
            tool_name: "shell".into(),
            input: serde_json::json!({"command": "ls"}),
        },
        80,
    );
    handle_harness_event(
        &mut app,
        HarnessEvent::ToolResult {
            tool_call_id: "c3".into(),
            tool_name: "shell".into(),
            output: serde_json::json!({"stdout": "", "exitCode": 0}),
            is_error: false,
        },
        80,
    );
    let joined = flat_lines(&activity_lines(&app, 80, true));
    assert!(joined.contains("Run    $ ls"));
    assert!(joined.contains("✓ Run    $ ls · exit 0"));
}

#[test]
fn wrapped_user_prompts_carry_the_spine_on_every_line() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    app.composer = "alpha beta gamma delta epsilon zeta eta theta".into();
    submit(&mut app, &tx, 24);
    let texts = pending_texts(&app);
    let prompt_lines: Vec<&String> = texts.iter().filter(|t| t.starts_with("┃ ")).collect();
    assert!(prompt_lines.len() >= 2, "prompt should wrap: {texts:?}");
    for line in prompt_lines {
        assert!(line.starts_with("┃ "), "spine carried: {line}");
    }
}

#[test]
fn bang_prompt_dispatches_a_shell_tool_in_the_workspace() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    app.composer = "!git status --short".into();
    app.cursor = app.composer.chars().count();

    submit(&mut app, &tx, 80);

    match rx.try_recv() {
        Ok(WorkerCmd::Shell {
            command,
            working_dir,
            ..
        }) => {
            assert_eq!(command, "git status --short");
            assert_eq!(working_dir, app.cfg.workspace_root);
        }
        other => panic!("expected shell command, got {:?}", other.is_ok()),
    }
    assert!(app.running());
    assert!(app.composer.is_empty());
    assert!(pending_texts(&app)
        .join("\n")
        .contains("!git status --short"));
}

#[test]
fn queued_bang_prompt_stays_a_shell_command() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    app.run = RunState::Running {
            id: crate::msg::RunId::User(1),
        started: Instant::now(),
        cancel: CancellationToken::new(),
    };
    app.prompt_queue.push_back("!pwd".into());

    handle_ui_msg(&mut app, UiMsg::RunDone { id: crate::msg::RunId::User(1), result: Ok(String::new()) }, &tx, 80);

    assert!(matches!(
        rx.try_recv(),
        Ok(WorkerCmd::Shell { command, .. }) if command == "pwd"
    ));
}

#[test]
fn shell_done_settles_the_tool_and_resets_the_run() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    app.run = RunState::Running {
            id: crate::msg::RunId::User(1),
        started: Instant::now(),
        cancel: CancellationToken::new(),
    };
    handle_harness_event(
        &mut app,
        HarnessEvent::ToolCall {
            tool_call_id: "user-shell-1".into(),
            tool_name: "shell".into(),
            input: serde_json::json!({"command": "pwd"}),
        },
        80,
    );
    handle_harness_event(
        &mut app,
        HarnessEvent::ToolResult {
            tool_call_id: "user-shell-1".into(),
            tool_name: "shell".into(),
            output: serde_json::json!({"stdout": "/test-ws\n", "exitCode": 0}),
            is_error: false,
        },
        80,
    );

    handle_ui_msg(&mut app, UiMsg::ShellDone { id: crate::msg::RunId::User(1) }, &tx, 80);

    assert!(!app.running());
    assert_eq!(
        app.tool_log.last().map(|tool| tool.tool_name.as_str()),
        Some("shell")
    );
}

#[test]
fn prompts_submitted_while_running_queue_in_fifo_order() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    app.run = RunState::Running {
            id: crate::msg::RunId::User(1),
        started: Instant::now(),
        cancel: CancellationToken::new(),
    };

    app.composer = "add queue rendering tests".into();
    submit(&mut app, &tx, 80);
    app.composer = "update the readme".into();
    submit(&mut app, &tx, 80);

    assert_eq!(
        app.prompt_queue.iter().cloned().collect::<Vec<_>>(),
        vec!["add queue rendering tests", "update the readme"]
    );
    assert!(app.composer.is_empty(), "queued input clears the composer");
    assert!(
        rx.try_recv().is_err(),
        "queued turns do not overlap the run"
    );
}

#[test]
fn successful_run_starts_the_next_queued_prompt() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    app.run = RunState::Running {
            id: crate::msg::RunId::User(1),
        started: Instant::now(),
        cancel: CancellationToken::new(),
    };
    app.prompt_queue.extend([
        "add queue rendering tests".to_string(),
        "update the readme".to_string(),
    ]);

    handle_ui_msg(&mut app, UiMsg::RunDone { id: crate::msg::RunId::User(1), result: Ok(String::new()) }, &tx, 80);

    match rx.try_recv() {
        Ok(WorkerCmd::Run { prompt, .. }) => {
            assert_eq!(prompt, "add queue rendering tests")
        }
        other => panic!("expected queued run, got {:?}", other.is_ok()),
    }
    assert!(app.running());
    assert_eq!(
        app.prompt_queue.iter().cloned().collect::<Vec<_>>(),
        vec!["update the readme"]
    );
    assert!(
        pending_texts(&app)
            .iter()
            .any(|line| line.contains("add queue rendering tests")),
        "a queued prompt enters the transcript when it starts"
    );
}

/// A finished turn reports its wall time and how many tool calls it made;
/// a failed or interrupted run does not.
#[test]
fn run_done_reports_turn_duration_and_tool_calls() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    app.run = RunState::Running {
            id: crate::msg::RunId::User(1),
        started: Instant::now(),
        cancel: CancellationToken::new(),
    };
    for id in ["c1", "c2"] {
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: id.into(),
                tool_name: "shell".into(),
                input: serde_json::json!({"command": "true"}),
            },
            80,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: id.into(),
                tool_name: "shell".into(),
                output: serde_json::json!(""),
                is_error: false,
            },
            80,
        );
    }
    app.turn_tokens_in = 12_004;
    app.turn_tokens_out = 611;
    handle_ui_msg(&mut app, UiMsg::RunDone { id: crate::msg::RunId::User(1), result: Ok(String::new()) }, &tx, 80);
    let texts = pending_texts(&app);
    let footer = texts
        .iter()
        .rev()
        .find(|t| !t.trim().is_empty())
        .expect("footer is the last transcript row");
    assert!(footer.starts_with("  done · "), "{footer}");
    assert!(footer.ends_with(" · 2 tools · ↑12.0k ↓611"), "{footer}");
    assert!(
        live_lines(&app, 80).is_empty(),
        "nothing floats above the composer once the turn is over"
    );

    let mut failed = test_app();
    failed.run = RunState::Running {
            id: crate::msg::RunId::User(1),
        started: Instant::now(),
        cancel: CancellationToken::new(),
    };
    handle_ui_msg(
        &mut failed,
        UiMsg::RunDone { id: crate::msg::RunId::User(1), result: Err("cancelled".into()) },
        &tx,
        80,
    );
    assert!(
        !pending_texts(&failed).iter().any(|t| t.contains("done · ")),
        "no footer on an interrupted run"
    );
}

#[test]
fn background_run_start_is_hidden_and_stale_completion_is_ignored() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    app.run = RunState::Running {
        id: crate::msg::RunId::User(9),
        started: Instant::now(),
        cancel: CancellationToken::new(),
    };
    let background = crate::msg::RunId::BackgroundProcess {
        generation: 2,
        sequence: 1,
    };

    handle_ui_msg(
        &mut app,
        UiMsg::RunStarted {
            id: background.clone(),
            cancel: CancellationToken::new(),
        },
        &tx,
        80,
    );
    assert!(matches!(
        &app.run,
        RunState::Running { id, .. } if id == &background
    ));
    assert!(
        pending_texts(&app).is_empty(),
        "background context stays out of the transcript"
    );

    handle_ui_msg(
        &mut app,
        UiMsg::RunDone {
            id: crate::msg::RunId::User(9),
            result: Ok(String::new()),
        },
        &tx,
        80,
    );
    assert!(matches!(
        &app.run,
        RunState::Running { id, .. } if id == &background
    ));
}
