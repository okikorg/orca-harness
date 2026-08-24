    #[test]
    fn failed_run_pauses_the_queue_until_empty_enter_resumes_it() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        app.prompt_queue.push_back("inspect the failure".into());

        handle_ui_msg(
            &mut app,
            UiMsg::RunDone(Err("model endpoint unavailable".into())),
            &tx,
            80,
        );

        assert!(!app.running());
        assert_eq!(
            app.prompt_queue.front().map(String::as_str),
            Some("inspect the failure")
        );
        assert!(
            rx.try_recv().is_err(),
            "failure must not cascade through the queue"
        );

        app.composer.clear();
        submit(&mut app, &tx, 80);
        match rx.try_recv() {
            Ok(WorkerCmd::Run { prompt, .. }) => assert_eq!(prompt, "inspect the failure"),
            other => panic!("expected resumed run, got {:?}", other.is_ok()),
        }
        assert!(app.prompt_queue.is_empty());
        assert!(app.running());
    }

    #[test]
    fn queue_rail_previews_three_prompts_and_collapses_overflow() {
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        app.prompt_queue.extend([
            "first queued prompt".to_string(),
            "second queued prompt".to_string(),
            "third queued prompt".to_string(),
            "fourth queued prompt".to_string(),
        ]);

        let rows = live_lines(&app, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(rows[0].contains("queued · 4"), "queue heading: {rows:?}");
        assert!(rows[1].contains("next") && rows[1].contains("first queued prompt"));
        assert!(rows[2].contains("2") && rows[2].contains("second queued prompt"));
        assert!(rows[3].contains("3") && rows[3].contains("third queued prompt"));
        assert!(rows[4].contains("+1 more"), "overflow summary: {rows:?}");
        assert!(
            rows[5].contains("working"),
            "spinner follows queue: {rows:?}"
        );
    }

    #[test]
    fn paused_queue_shows_resume_guidance_in_the_composer_and_status() {
        let mut app = App::new(TuiConfig {
            model_name: "test".into(),
            workspace_name: "/workspace".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        });
        app.prompt_queue.push_back("inspect the failure".into());

        let screen = rendered_rows(&mut app, 100, 24).join("\n");
        assert!(screen.contains("queue paused · enter to resume"));
        assert!(screen.contains("queued · 1"));
        assert!(screen.contains("queued 1 · enter resume · /queue clear"));
    }

    #[test]
    fn queue_clear_discards_waiting_prompts_without_interrupting_the_run() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        app.prompt_queue.extend(["one".into(), "two".into()]);
        app.composer = "/queue clear".into();

        submit(&mut app, &tx, 80);

        assert!(app.prompt_queue.is_empty());
        assert!(
            app.running(),
            "clearing the queue leaves the current run alone"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn compact_command_reaches_the_worker_and_reports_the_result() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "/compact".into();
        submit(&mut app, &tx, 80);
        assert!(
            matches!(rx.try_recv(), Ok(WorkerCmd::Compact)),
            "/compact sends the worker command"
        );

        let report = orca_harness_extensions::CompactReport {
            messages_before: 41,
            messages_after: 2,
            bytes_before: 130_574,
            bytes_after: 1_264,
            est_tokens_before: 32_643,
            est_tokens_after: 316,
            head_messages: 40,
            tail_messages: 0,
            elided_results: 16,
            elided_bytes: 116_177,
            elided_call_ids: vec!["c1".into()],
            summary: "summary".into(),
            files_read: vec![],
            files_modified: vec![],
        };
        app.context_tokens = 32_643;
        handle_ui_msg(&mut app, UiMsg::Compacted(Ok(report)), &tx, 80);
        let text = app
            .pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("compacted: 41 -> 2 messages"), "{text}");
        assert!(text.contains("16 tool outputs"), "{text}");
        assert!(text.contains("recoverable via read_tool_result"), "{text}");
        assert_eq!(
            app.context_tokens, 316,
            "the status-line context meter reflects the compacted size"
        );
    }

    #[test]
    fn context_meter_tracks_the_latest_step_not_the_session_total() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        for (input, output) in [(1_000, 50), (1_200, 80)] {
            handle_ui_msg(
                &mut app,
                UiMsg::Event(HarnessEvent::Usage {
                    usage: orca_harness_core::Usage {
                        input_tokens: input,
                        output_tokens: output,
                        cache_read_tokens: 0,
                        cache_create_tokens: 0,
                    },
                }),
                &tx,
                80,
            );
        }
        assert_eq!(app.tokens_in, 2_200, "session total accumulates");
        assert_eq!(
            app.context_tokens, 1_280,
            "context meter is the latest step's input + output"
        );

        // A tool result lands before the next model step: pi-style
        // trailing estimate (bytes/4) until real usage overwrites it.
        let output = serde_json::json!({"content": "x".repeat(396)});
        let bytes = serde_json::to_string(&output).unwrap().len() as u64;
        handle_ui_msg(
            &mut app,
            UiMsg::Event(HarnessEvent::ToolResult {
                tool_call_id: "c9".into(),
                tool_name: "read_file".into(),
                output,
                is_error: false,
            }),
            &tx,
            80,
        );
        assert_eq!(app.context_tokens, 1_280 + bytes / 4);
    }

    #[test]
    fn context_segment_shows_percentage_when_the_window_is_known() {
        assert_eq!(context_segment(41_881, Some(128_000)), "ctx 32%");
        assert_eq!(context_segment(500, None), "ctx ~500");
        assert_eq!(context_segment(2_350, Some(0)), "ctx ~2.4k");
    }

    #[test]
    fn slash_usage_opens_a_read_only_tray_and_esc_closes_it() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.tokens_in = 250_798;
        app.tokens_out = 3_609;
        app.cache_read_total = 12;
        app.cache_write_total = 3;
        app.usage_steps = 6;
        app.context_tokens = 41_881;
        app.context_window = Some(128_000);
        slash_command(&mut app, "usage", &tx, 100);
        assert!(matches!(app.overlay, Some(Overlay::Usage)));

        let text = flat_lines(&live_lines(&app, 100));
        assert!(text.contains("Session usage"), "{text}");
        assert!(text.contains("41881 / 128000 (32%)"), "{text}");
        assert!(text.contains("input        250798"), "{text}");
        assert!(text.contains("cache read   12"), "{text}");
        assert!(text.contains("total        254422"), "{text}");
        assert!(text.contains("model steps  6"), "{text}");

        press(&mut app, &tx, KeyCode::Esc);
        assert!(app.overlay.is_none(), "esc dismisses the tray");
        assert!(rx.try_recv().is_err(), "the tray never talks to the worker");
    }

    #[test]
    fn later_turns_have_no_divider_or_trailing_spine() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();

        app.composer = "first turn".into();
        submit(&mut app, &tx, 40);
        app.run = RunState::Idle;
        app.composer = "second turn".into();
        submit(&mut app, &tx, 40);

        let texts = pending_texts(&app);
        assert!(
            !texts.iter().any(|line| line.starts_with("  ─")),
            "turn divider removed: {texts:?}"
        );
        assert!(
            !texts.iter().any(|line| line == "┃"),
            "spine ends with prompt text: {texts:?}"
        );
        let second = texts
            .iter()
            .position(|line| line == "┃ second turn")
            .expect("second prompt");
        assert_eq!(texts[second - 1], "", "one row separates turns: {texts:?}");
        assert!(
            second < 2 || !texts[second - 2].is_empty(),
            "spacing stays to one row: {texts:?}"
        );
        assert_eq!(app.turn_count, 2);
    }

    #[test]
    fn approval_verdicts_align_under_the_call() {
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
        let (respond, _rx) = tokio::sync::oneshot::channel();
        app.approval = Some(crate::msg::ApprovalRequest {
            tool_name: "shell".into(),
            detail: "shell $ ls".into(),
            respond,
        });
        handle_approval_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        );
        let joined = flat_lines(&activity_lines(&app, 80, true));
        assert!(joined.contains("shell $ ls"));
        assert!(joined.contains("□ shell $ ls · approved"));
    }

    #[test]
    fn clear_flushes_the_screen_and_resets_session_state() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.scroll = 20;
        app.tokens_in = 100;
        app.spinner_frame = 99;
        app.prompt_queue.push_back("waiting prompt".into());
        app.tool_log.push(ToolRecord {
            call_line: "shell $ ls".into(),
            tool_name: "shell".into(),
            output: serde_json::json!({}),
            inner: Vec::new(),
        });

        slash_command(&mut app, "clear", &tx, 80);

        assert!(app.transcript.is_empty(), "transcript wiped");
        assert_eq!(app.scroll, 0);
        assert_eq!(app.tokens_in, 0);
        assert!(app.prompt_queue.is_empty(), "prompt queue wiped");
        assert!(app.tool_log.is_empty(), "expandable log wiped");
        assert!(
            matches!(rx.try_recv(), Ok(WorkerCmd::Clear)),
            "worker told to reset the context"
        );
        assert!(
            app.pending_history.is_empty(),
            "clear leaves an empty transcript"
        );
        let screen = rendered_rows(&mut app, 80, 24).join("\n");
        assert!(
            !screen.contains("ORCA HARNESS"),
            "welcome should stay removed: {screen}"
        );
    }

    #[test]
    fn other_mouse_events_are_ignored() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        handle_terminal_event(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left)),
            &tx,
            80,
        );
        assert_eq!(app.scroll, 0);
        assert!(app.composer.is_empty());
    }

    #[test]
    fn live_activity_groups_thinking_and_parallel_tools() {
        let mut app = test_app();
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        handle_harness_event(
            &mut app,
            HarnessEvent::ReasoningDelta {
                text: "Inspecting the event flow".into(),
            },
            100,
        );
        for (id, name, input) in [
            ("c1", "read_file", serde_json::json!({"path": "src/tui.rs"})),
            ("c2", "shell", serde_json::json!({"command": "cargo test"})),
        ] {
            handle_harness_event(
                &mut app,
                HarnessEvent::ToolCall {
                    tool_call_id: id.into(),
                    tool_name: name.into(),
                    input,
                },
                100,
            );
        }
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "read_file".into(),
                output: serde_json::json!({"bytes": 2048, "content": "..."}),
                is_error: false,
            },
            100,
        );

        let joined = projected_transcript(&app, 100)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("Thinking"),
            "thinking group missing: {joined}"
        );
        assert!(
            joined.contains("Work · ✓ 1 · □ 1"),
            "work totals missing: {joined}"
        );
        assert!(
            joined.contains("read_file src/tui.rs"),
            "completed call missing: {joined}"
        );
        assert!(
            joined.contains("✓ read_file src/tui.rs · read 2048 bytes"),
            "result missing: {joined}"
        );
        assert!(
            joined.contains("shell $ cargo test"),
            "running call missing: {joined}"
        );
        assert!(joined.contains("□"), "running state missing: {joined}");
    }

    #[test]
    fn live_activity_prioritizes_running_tools_and_bounds_the_history() {
        let mut app = test_app();
        for index in 0..12 {
            app.activity_tools.push(ToolActivity {
                call_id: format!("call-{index}"),
                call_line: format!("read_file file-{index}.rs"),
                tool_name: "read_file".into(),
                input: serde_json::json!({"path": format!("file-{index}.rs")}),
                started: Instant::now(),
                elapsed: Some(Duration::from_millis(1)),
                output: Some(serde_json::json!({"bytes": 42})),
                is_error: false,
                approval: None,
            });
        }
        app.activity_tools.push(ToolActivity {
            call_id: "call-shell".into(),
            call_line: "shell $ cargo test --workspace".into(),
            tool_name: "shell".into(),
            input: serde_json::json!({"command": "cargo test --workspace"}),
            started: Instant::now(),
            elapsed: None,
            output: None,
            is_error: false,
            approval: None,
        });

        let rendered = activity_lines(&app, 100, true);
        let joined = rendered
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("… 5 earlier tools"),
            "history summarized: {joined}"
        );
        assert!(
            joined.contains("□ shell $ cargo test --workspace"),
            "running tool retained: {joined}"
        );
        assert!(
            !joined.contains("file-0.rs"),
            "oldest tools hidden: {joined}"
        );
        assert!(rendered.len() <= LIVE_TOOL_ROWS + 2, "rail stays bounded");
    }

    #[test]
    fn completed_run_keeps_activity_expanded_before_the_answer() {
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ReasoningDelta {
                text: "private reasoning text".into(),
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "shell".into(),
                input: serde_json::json!({"command": "cargo test"}),
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "shell".into(),
                output: serde_json::json!({"stdout": "42 tests passed", "exitCode": 0}),
                is_error: false,
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::Result {
                message: "Everything passed.".into(),
            },
            100,
        );

        let texts = pending_texts(&app);
        let joined = texts.join("\n");
        let work = joined.find("Work · 1 tool").expect("work rail");
        let answer = joined.find("Everything passed.").expect("answer");
        assert!(work < answer, "work precedes answer: {joined}");
        assert!(joined.contains("shell $ cargo test"));
        assert!(joined.contains("✓ shell $ cargo test · exit 0 · 42 tests passed"));
        assert!(
            !joined.contains("private reasoning text"),
            "completed thinking is collapsed"
        );

        let details = flat_lines(&app.work_log.last().expect("work tree retained").lines);
        assert!(details.contains("Thinking"));
        assert!(details.contains("shell $ cargo test"));
        assert!(details.contains("✓ shell $ cargo test · exit 0 · 42 tests passed"));
        assert!(
            app.work_log.last().expect("work tree retained").expanded,
            "completed rails are expanded by default"
        );

        app.absorb_pending();
        assert!(expand_latest_work(&mut app));
        let expanded = app
            .transcript
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded.contains("shell $ cargo test"));
        let tool = expanded.find("shell $ cargo test").expect("expanded tool");
        let answer = expanded.find("Everything passed.").expect("answer");
        assert!(tool < answer, "work expands in place: {expanded}");
        let once = app.transcript.len();
        assert!(expand_latest_work(&mut app));
        assert_eq!(app.transcript.len(), once, "repeat expansion is a no-op");
    }

