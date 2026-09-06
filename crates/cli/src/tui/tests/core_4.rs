    #[test]
    fn transcript_preserves_model_tool_phase_chronology() {
        let mut app = test_app();

        handle_harness_event(
            &mut app,
            HarnessEvent::ReasoningDelta {
                text: "inspect the extension trait".into(),
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::Assistant {
                message: "I understand the core mechanism. I will inspect the built-ins.\n\n"
                    .into(),
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "read_file".into(),
                input: serde_json::json!({"path": "crates/extensions/src/lib.rs"}),
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "read_file".into(),
                output: serde_json::json!({"bytes": 2048}),
                is_error: false,
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ReasoningDelta {
                text: "compare the concrete implementations".into(),
            },
            100,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::Result {
                message: "The extension mechanism is useful.".into(),
            },
            100,
        );

        let texts = pending_texts(&app);
        let joined = texts.join("\n");
        let first_thinking = joined.find("Thinking ·").expect("first thinking phase");
        let checkpoint = joined
            .find("I understand the core mechanism")
            .expect("checkpoint");
        let work = joined.find("Work · 1 tool").expect("tool phase");
        let second_thinking = joined
            .match_indices("Thinking ·")
            .nth(1)
            .map(|(index, _)| index)
            .expect("second thinking phase");
        let answer = joined
            .find("The extension mechanism is useful.")
            .expect("answer");

        assert!(
            first_thinking < checkpoint
                && checkpoint < work
                && work < second_thinking
                && second_thinking < answer,
            "event chronology retained: {joined}"
        );
        let checkpoint_row = texts
            .iter()
            .position(|line| line.contains("I understand the core mechanism"))
            .expect("checkpoint row");
        let second_thinking_row = texts
            .iter()
            .rposition(|line| line.contains("Thinking ·"))
            .expect("second thinking row");
        assert!(
            texts[checkpoint_row + 1..=second_thinking_row]
                .iter()
                .any(|line| line.is_empty()),
            "successive prose and rails have one-row breathing room: {texts:?}"
        );
        assert_eq!(app.work_log.len(), 3, "each phase remains expandable");
    }

    #[test]
    fn transcript_block_component_owns_vertical_rhythm() {
        let mut app = test_app();
        app.push_line(Line::from("prompt"));

        app.push_markdown_block(
            "\nfirst paragraph\n\nsecond paragraph\n\n",
            80,
            BlockSpacing::Tight,
        );
        app.push_transcript_block(
            vec![Line::from(""), Line::from("work"), Line::from("")],
            BlockSpacing::Tight,
        );
        app.push_markdown_block("final answer\n", 80, BlockSpacing::Section);

        assert_eq!(
            pending_texts(&app),
            vec![
                "prompt",
                "",
                "  first paragraph",
                "",
                "  second paragraph",
                "work",
                "",
                "  final answer",
            ]
        );
    }

    #[test]
    fn transcript_has_no_role_labels() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "Explain the change".into();
        submit(&mut app, &tx, 80);
        handle_harness_event(
            &mut app,
            HarnessEvent::Result {
                message: "Here is the change.".into(),
            },
            80,
        );
        let joined = pending_texts(&app).join("\n");
        let prompt = joined.find("Explain the change").expect("prompt");
        let answer = joined.find("Here is the change.").expect("answer");
        assert!(prompt < answer, "turn order: {joined}");
        assert!(!joined.contains("YOU"), "user label removed: {joined}");
        assert!(
            !joined.contains("ORCA"),
            "assistant label removed: {joined}"
        );
    }

    #[test]
    fn edit_activity_keeps_its_diff_preview() {
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "edit_file".into(),
                input: serde_json::json!({"path": "src/lib.rs", "old": "let x = 1;", "new": "let x = 2;"}),
            },
            100,
        );
        let joined = flat_lines(&activity_lines(&app, 100, true));
        assert!(
            joined.contains("- let x = 1;"),
            "removed line visible: {joined}"
        );
        assert!(
            joined.contains("+ let x = 2;"),
            "added line visible: {joined}"
        );
    }

    #[test]
    fn selected_tool_keeps_elapsed_time_next_to_the_call() {
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "shell".into(),
                input: serde_json::json!({"command": "cargo test --workspace"}),
            },
            180,
        );
        app.activity_tools[0].started = Instant::now() - Duration::from_secs(2);

        let joined = flat_lines(&activity_lines_selected(&app, 180, true, Some(0)));
        assert!(
            joined.contains("Run · $ cargo test --workspace · 2.0s"),
            "elapsed follows the call without an alignment gap: {joined}"
        );
    }

    #[test]
    fn multiline_edit_preview_is_source_shaped_and_bounded() {
        let mut app = test_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "edit_file".into(),
                input: serde_json::json!({
                    "path": "src/lib.rs",
                    "old": "fn old() {\n    one();\n    two();\n    three();\n}",
                    "new": "fn new() {\n    four();\n    five();\n}"
                }),
            },
            100,
        );

        let rendered = activity_lines(&app, 100, true);
        let joined = flat_lines(&rendered);
        assert!(joined.contains("- fn old() {"), "old source: {joined}");
        assert!(joined.contains("-     one();"), "indentation: {joined}");
        assert!(joined.contains("+ fn new() {"), "new source: {joined}");
        assert!(
            joined.contains("… 3 more changed lines"),
            "bounded preview: {joined}"
        );
        assert_eq!(
            rendered
                .iter()
                .filter(|line| {
                    let text = line
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>();
                    text.contains(" - ") || text.contains(" + ")
                })
                .count(),
            6,
            "only the preview budget is rendered"
        );
    }

    #[test]
    fn failed_tool_expands_its_output_in_the_rail() {
        let mut app = test_app();
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
                output: serde_json::json!({"stdout": "test result: FAILED", "stderr": "assertion failed", "exitCode": 1}),
                is_error: true,
            },
            100,
        );
        let joined = flat_lines(&activity_lines(&app, 100, true));
        assert!(
            joined.contains("× Run · $ cargo test") && joined.contains("Exit 1"),
            "failure state shown: {joined}"
        );
        assert!(
            app.activity_tools[0].output.as_ref().unwrap()["stdout"] == "test result: FAILED",
            "full stdout remains available for inspection: {joined}"
        );
        assert!(
            joined.contains("assertion failed"),
            "stderr expanded: {joined}"
        );
    }

    #[test]
    fn immediate_model_failure_renders_without_assistant_label() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.run = RunState::Running {
            id: crate::msg::RunId::User(1),
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        handle_ui_msg(
            &mut app,
            UiMsg::RunDone { id: crate::msg::RunId::User(1), result: Err("model endpoint unavailable".into()) },
            &tx,
            80,
        );
        let joined = pending_texts(&app).join("\n");
        assert!(joined.contains("run failed:"), "failure message: {joined}");
        assert!(
            !joined.contains("ORCA"),
            "assistant label removed: {joined}"
        );
    }

    /// Whether the first cell of `needle` on screen carries the bold modifier.
    fn rendered_cell_is_bold(app: &mut App, width: u16, height: u16, needle: &str) -> bool {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .find(|&(x, y)| {
                (x..width)
                    .map(|col| buffer[(col, y)].symbol())
                    .collect::<String>()
                    .starts_with(needle)
            })
            .is_some_and(|(x, y)| buffer[(x, y)].modifier.contains(ratatui::style::Modifier::BOLD))
    }

    fn rendered_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    include!("core_4_welcome_help.rs");

    /// The rendered status line, which is the row carrying the model name.
    fn status_row(app: &mut App) -> String {
        rendered_rows(app, 100, 24)
            .into_iter()
            .find(|row| row.contains(&app.cfg.model_name))
            .expect("status line")
    }

    #[test]
    fn copying_the_inspected_tool_leaves_the_transcript_out() {
        let mut app = test_app();
        app.transcript
            .push(Line::from("the answer text in the left pane"));
        app.last_answer = Some("the answer text in the left pane".into());
        app.activity_tools.push(ToolActivity {
            call_id: "call-shell".into(),
            call_line: "shell $ cargo run --release -p orcacode".into(),
            tool_name: "shell".into(),
            input: serde_json::json!({"command": "cargo run --release -p orcacode"}),
            started: Instant::now(),
            execution_started: None,
            execution_elapsed: None,
            elapsed: None,
            output: Some(serde_json::json!({"text": "compiling"})),
            is_error: false,
            approval: None,
        });
        app.split_tool = Some(0);

        copy_command(&mut app, "tool");
        let copied = app.clipboard_pending.take().expect("tool copy");

        assert!(
            copied.contains("cargo run --release -p orcacode") && copied.contains("compiling"),
            "inspector pane should come out whole: {copied}"
        );
        assert!(
            !copied.contains("the answer text in the left pane"),
            "the other pane should stay out of it: {copied}"
        );
    }

    #[test]
    fn scrolling_offers_the_selection_hint_then_settles_back() {
        let mut app = test_app();
        app.turn_count = 1;
        for row in 0..40 {
            app.transcript.push(Line::from(format!("row {row}")));
        }

        scroll_transcript(&mut app, 5);
        let status = status_row(&mut app);
        assert!(
            status.contains("opt/shift+drag selects") && status.contains("ctrl+y copies"),
            "fresh scroll should offer both ways out: {status}"
        );

        // Past its window the hint gives the status line back.
        app.scroll_hint_at = Some(Instant::now() - SCROLL_HINT - Duration::from_secs(1));
        let status = status_row(&mut app);
        assert!(
            status.contains("scrolled · pgdn to follow"),
            "hint should settle back: {status}"
        );
        assert!(!app.scroll_hint_live(), "expired hint should stop the tick");
    }

    #[test]
    fn status_line_starts_with_model_and_anchors_workspace_and_branch() {
        let mut app = App::new(TuiConfig {
            model_name: "gpt-oss:20b".into(),
            workspace_name: "/workspace/orca-harness".into(),
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
        app.git_branch = Some("feature/status-branch".into());
        app.turn_count = 1;

        let rows = rendered_rows(&mut app, 160, 24);
        let status = rows
            .iter()
            .find(|row| row.contains("gpt-oss:20b"))
            .expect("status line");

        assert!(
            status.starts_with(" local:gpt-oss:20b ·"),
            "model not first: {status}"
        );
        assert!(
            status
                .trim_end()
                .ends_with("  orca-harness · feature/status-branch"),
            "workspace and branch not anchored right: {status}"
        );
        assert!(
            !status.contains("cwd"),
            "cwd prefix should be omitted: {status}"
        );
        assert!(
            !status.contains("/workspace/"),
            "full path should be omitted: {status}"
        );
    }
