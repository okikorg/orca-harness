    #[test]
    fn inspector_write_input_renders_source_instead_of_escaped_json() {
        let tool = ToolActivity {
            call_id: "write-1".into(),
            call_line: "write_file README.md".into(),
            tool_name: "write_file".into(),
            input: serde_json::json!({
                "path": "README.md",
                "content": "# Orca\n\n    indented code\n"
            }),
            started: Instant::now(),
            elapsed: Some(Duration::from_millis(1)),
            output: Some(serde_json::json!({"path": "README.md", "bytesWritten": 26})),
            is_error: false,
            approval: None,
        };
        let inspector = flat_lines(&tool_inspector_lines(&tool, 80));
        assert!(inspector.contains("README.md · markdown · 3 lines · 26 B"));
        assert!(inspector.contains("# Orca"));
        assert!(inspector.contains("    indented code"));
        assert!(!inspector.contains("\\n"));
        assert!(inspector.contains("README.md · wrote 26 B"));
    }

    #[test]
    fn inspector_output_with_tabs_and_ansi_renders_clean_cells() {
        // du/ls emit tab-separated columns and some tools emit ANSI color;
        // raw control bytes in a cell desync the terminal cursor from the
        // draw buffer and leave ghost cells behind.
        let tool = ToolActivity {
            call_id: "shell-1".into(),
            call_line: "shell $ du -sh ~/.nvm/*".into(),
            tool_name: "shell".into(),
            input: serde_json::json!({"command": "du -sh ~/.nvm/*"}),
            started: Instant::now(),
            elapsed: Some(Duration::from_millis(1)),
            output: Some(serde_json::json!({
                "stdout": "205M\t/Users/akashswamy/.nvm/versions\n\u{1b}[31m12K\u{1b}[0m\t/tmp/x\n",
                "stderr": "",
                "exitCode": 0
            })),
            is_error: false,
            approval: None,
        };
        for line in tool_inspector_lines(&tool, 80) {
            for span in &line.spans {
                assert!(
                    !span.content.contains(|c: char| c.is_control()),
                    "control byte reached a cell: {:?}",
                    span.content
                );
            }
        }
    }

    #[test]
    fn inspector_json_preview_stops_after_one_level() {
        let output = serde_json::json!({
            "ok": true,
            "metadata": { "owner": { "name": "orca" }, "count": 3 },
            "results": [{ "id": 1 }, { "id": 2 }]
        });
        let (preview, omitted) = shallow_json_preview(&output);
        assert!(!omitted);
        assert!(preview.contains("\"ok\": true"));
        assert!(preview.contains("\"metadata\": { … }"));
        assert!(preview.contains("\"results\": [ … ]"));
        assert!(!preview.contains("owner"));
        assert!(!preview.contains("id"));
    }

    fn pending_texts(app: &App) -> Vec<String> {
        app.pending_history
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn notifications_use_the_shared_leading_glyph() {
        let _theme = THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let mut app = test_app();

        push_notice(&mut app, "theme set to default");

        let notice = app.pending_history.last().expect("notification line");
        assert_eq!(line_text(notice), "• theme set to default");
        assert_eq!(notice.spans[0].style, theme().accent);
        assert_eq!(notice.spans[1].style, theme().dim);
    }

    /// A refused command is still the system talking. Without the shared
    /// glyph it renders flush-left against the notices around it and
    /// reads as model output — so the glyph is the same and only the
    /// body carries the error color.
    #[test]
    fn errors_use_the_same_glyph_as_notices_with_an_error_body() {
        let _theme = THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let mut app = test_app();

        push_notice(&mut app, "plan mode · read-only");
        push_error(&mut app, "unknown mode: pkan");

        let notice = &app.pending_history[app.pending_history.len() - 2];
        let error = app.pending_history.last().expect("error line");
        assert_eq!(line_text(error), "• unknown mode: pkan");
        // Same leading glyph, so both lines start in the same column.
        assert_eq!(line_text(notice).chars().next(), Some('•'));
        assert_eq!(error.spans[0].style, notice.spans[0].style);
        // Severity is the body's job, and it differs from a notice.
        assert_eq!(error.spans[1].style, theme().error);
        assert_ne!(error.spans[1].style, notice.spans[1].style);
    }

    #[test]
    fn elapsed_labels_keep_sub_millisecond_tool_timings_visible() {
        assert_eq!(elapsed_label(Duration::ZERO), "0ns");
        assert_eq!(elapsed_label(Duration::from_nanos(850)), "850ns");
        assert_eq!(elapsed_label(Duration::from_micros(842)), "842µs");
        assert_eq!(elapsed_label(Duration::from_millis(14)), "14ms");
        assert_eq!(elapsed_label(Duration::from_millis(1_500)), "1.5s");
    }

    #[test]
    fn tool_results_connect_under_their_calls() {
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "shell".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            80,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "shell".into(),
                output: serde_json::json!({"stdout": "a.rs", "exitCode": 0}),
                is_error: false,
            },
            80,
        );
        let joined = flat_lines(&activity_lines(&app, 80, true));
        assert!(joined.contains("shell $ ls"));
        assert!(joined.contains("✓ shell $ ls · exit 0 · a.rs"));
    }

    fn flat_lines(lines: &[Line]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect()
    }

    #[test]
    fn streaming_reasoning_renders_inside_the_thinking_group() {
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        app.reasoning = "secret chain of thought".into();

        let joined = flat_lines(&projected_transcript(&app, 80));
        assert!(
            joined.contains("Thinking"),
            "thinking group shown: {joined}"
        );
        assert!(
            joined.contains("secret chain of thought"),
            "reasoning tail shown: {joined}"
        );

        // Actual answer text still streams live.
        app.text = "partial answer".into();
        let projected = projected_transcript(&app, 80);
        let joined = flat_lines(&projected);
        assert!(joined.contains("partial answer"));
        let answer_row = projected
            .iter()
            .position(|line| line_text(line).contains("partial answer"))
            .expect("partial answer row");
        assert!(
            answer_row > 0 && line_is_blank(&projected[answer_row - 1]),
            "thinking and live prose need one blank row: {:?}",
            projected.iter().map(line_text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn thinking_joins_the_rail_and_the_expand_log() {
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ReasoningDelta {
                text: "let me check the file".into(),
            },
            80,
        );
        // A tool call ends the thinking phase even with no assistant text.
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "read_file".into(),
                input: serde_json::json!({"path": "a.rs"}),
            },
            80,
        );
        let joined = flat_lines(&activity_lines(&app, 80, true));
        assert!(
            joined.contains("Thinking"),
            "thinking group first: {joined}"
        );
        assert!(joined.contains("read_file a.rs"));
        let record = app.tool_log.last().expect("thinking recorded");
        assert_eq!(record.tool_name, "thinking");
        assert_eq!(record.output, serde_json::json!("let me check the file"));
        assert!(app.reasoning.is_empty(), "buffer reset after flush");
    }

    #[test]
    fn interrupted_thinking_still_lands_in_the_rail() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ReasoningDelta { text: "hmm".into() },
            80,
        );
        handle_ui_msg(&mut app, UiMsg::RunDone(Err("cancelled".into())), &tx, 80);
        let texts = pending_texts(&app);
        assert!(
            texts.iter().any(|t| t.contains("Thinking ·")),
            "partial thinking summarized: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("hmm")),
            "committed thinking collapsed"
        );
        assert_eq!(app.tool_log.last().unwrap().tool_name, "thinking");
        let details = flat_lines(&app.work_log.last().expect("work tree retained").lines);
        assert!(
            details.contains("Thinking"),
            "work tree retained: {details}"
        );
    }

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
        let list_call = joined.find("list_dir .").unwrap();
        let list_result = joined.find("· 2 entries").unwrap();
        let shell_call = joined.find("shell $ git log").unwrap();
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
        assert!(joined.contains("shell $ ls"));
        assert!(joined.contains("✓ shell $ ls · exit 0"));
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
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        app.prompt_queue.push_back("!pwd".into());

        handle_ui_msg(&mut app, UiMsg::RunDone(Ok(String::new())), &tx, 80);

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

        handle_ui_msg(&mut app, UiMsg::ShellDone, &tx, 80);

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
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        app.prompt_queue.extend([
            "add queue rendering tests".to_string(),
            "update the readme".to_string(),
        ]);

        handle_ui_msg(&mut app, UiMsg::RunDone(Ok(String::new())), &tx, 80);

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
        handle_ui_msg(&mut app, UiMsg::RunDone(Ok(String::new())), &tx, 80);
        let summary = app.last_turn_summary.clone().expect("summary recorded");
        assert!(summary.starts_with("Turn took"), "{summary}");
        assert!(summary.contains("s and took 2 tool calls"), "{summary}");
        assert!(
            !pending_texts(&app).iter().any(|t| t.contains("Turn took")),
            "summary stays out of the transcript"
        );
        let rail: Vec<String> = live_lines(&app, 80)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            rail.iter().any(|t| t.contains(&summary)),
            "summary rendered in the rail above the composer: {rail:?}"
        );

        let mut failed = test_app();
        failed.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        handle_ui_msg(
            &mut failed,
            UiMsg::RunDone(Err("cancelled".into())),
            &tx,
            80,
        );
        assert!(
            failed.last_turn_summary.is_none(),
            "no summary on an interrupted run"
        );
    }

