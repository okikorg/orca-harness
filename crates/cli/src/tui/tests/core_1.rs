    use super::*;
    use super::input::insert_image_bytes_for_test;
    use super::state::HeldInput;
    use orca_harness_model_providers::openrouter::ModelInfo;
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::backend::TestBackend;
    use ratatui::style::Style;

    fn paste(app: &mut App, tx: &mpsc::UnboundedSender<WorkerCmd>, text: &str) {
        handle_terminal_event(app, CtEvent::Paste(text.to_string()), tx, 80);
    }

    /// The bug this replaced: without bracketed paste every newline
    /// arrived as enter, so a pasted block submitted its first line and
    /// queued the rest.
    #[test]
    fn a_multiline_paste_is_one_marker_and_submits_nothing() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        let block = (1..=23)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");

        paste(&mut app, &tx, &block);

        assert_eq!(app.composer, "[Pasted text #1, 23 lines]");
        assert_eq!(app.cursor, app.composer.chars().count());
        assert!(app.prompt_queue.is_empty(), "paste must not queue prompts");
        assert!(rx.try_recv().is_err(), "paste must not start a run");
    }

    #[test]
    fn a_marker_expands_to_the_held_text_on_send() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();

        paste(&mut app, &tx, "alpha\nbeta\ngamma");
        for c in " explain this".chars() {
            press(&mut app, &tx, KeyCode::Char(c));
        }
        assert_eq!(app.composer, "[Pasted text #1, 3 lines] explain this");
        submit(&mut app, &tx, 80);

        match rx.try_recv() {
            Ok(WorkerCmd::Run { prompt, .. }) => {
                assert_eq!(prompt, "alpha\nbeta\ngamma explain this");
            }
            other => panic!("expected a run, got {:?}", other.is_ok()),
        }
        // Once sent, the turn shows what the model got, not the marker.
        let shown = app
            .pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            shown.contains("alpha") && shown.contains("beta") && shown.contains("gamma"),
            "transcript should unfurl the paste: {shown}"
        );
        assert!(
            !shown.contains("[Pasted text #"),
            "no marker should survive into the transcript: {shown}"
        );
    }

    #[test]
    fn backspace_removes_a_marker_whole() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        for c in "look at ".chars() {
            press(&mut app, &tx, KeyCode::Char(c));
        }
        paste(&mut app, &tx, "alpha\nbeta\ngamma");

        press(&mut app, &tx, KeyCode::Backspace);

        assert_eq!(app.composer, "look at ");
        assert_eq!(app.cursor, 8);
    }

    #[test]
    fn delete_removes_a_marker_whole_from_its_start() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        paste(&mut app, &tx, "alpha\nbeta");
        for c in " done".chars() {
            press(&mut app, &tx, KeyCode::Char(c));
        }
        app.cursor = 0;

        press(&mut app, &tx, KeyCode::Delete);

        assert_eq!(app.composer, " done");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn arrows_step_over_a_marker_rather_than_into_it() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        paste(&mut app, &tx, "alpha\nbeta");
        let width = app.composer.chars().count();

        press(&mut app, &tx, KeyCode::Left);
        assert_eq!(app.cursor, 0, "left clears the whole marker");

        press(&mut app, &tx, KeyCode::Right);
        assert_eq!(app.cursor, width, "right clears the whole marker");
    }

    /// Backspacing the chip must not strand the next paste's numbering.
    #[test]
    fn a_removed_marker_leaves_later_markers_expanding() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        paste(&mut app, &tx, "first\nblock");
        press(&mut app, &tx, KeyCode::Backspace);
        assert_eq!(app.composer, "");

        paste(&mut app, &tx, "second\nblock");
        assert_eq!(app.composer, "[Pasted text #2, 2 lines]");
        submit(&mut app, &tx, 80);

        match rx.try_recv() {
            Ok(WorkerCmd::Run { prompt, .. }) => assert_eq!(prompt, "second\nblock"),
            other => panic!("expected a run, got {:?}", other.is_ok()),
        }
    }

    fn add_test_image(app: &mut App) {
        insert_image_bytes_for_test(app, b"\x89PNG\r\n\x1a\nminimal".to_vec());
    }

    #[test]
    fn clipboard_image_is_an_atomic_pill_and_sends_native_data() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.cfg.provider = Provider::OpenAi;
        add_test_image(&mut app);

        assert_eq!(app.composer, "[▧ image.png]");
        let pill_end = app.cursor;
        press(&mut app, &tx, KeyCode::Left);
        assert_eq!(app.cursor, 0);
        press(&mut app, &tx, KeyCode::Right);
        assert_eq!(app.cursor, pill_end);

        submit(&mut app, &tx, 80);
        match rx.try_recv() {
            Ok(WorkerCmd::Run { prompt, images, .. }) => {
                assert_eq!(prompt, "[▧ image.png]");
                assert_eq!(images.len(), 1);
                assert_eq!(images[0].media_type, "image/png");
            }
            other => panic!("expected a run, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn backspace_removes_an_image_pill_whole() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        add_test_image(&mut app);

        press(&mut app, &tx, KeyCode::Backspace);
        assert_eq!(app.composer, "");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn delete_removes_an_image_pill_whole_from_its_start() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        add_test_image(&mut app);
        app.cursor = 0;

        press(&mut app, &tx, KeyCode::Delete);
        assert_eq!(app.composer, "");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn pasted_image_path_remains_plain_text() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();

        paste(&mut app, &tx, "/tmp/screenshot.png");
        assert_eq!(app.composer, "/tmp/screenshot.png");
        assert!(app.pastes.is_empty());
    }

    #[test]
    fn local_model_keeps_image_draft_when_send_is_blocked() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        add_test_image(&mut app);
        let draft = app.composer.clone();

        submit(&mut app, &tx, 80);
        assert_eq!(app.composer, draft);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn repeated_clipboard_images_get_distinct_pills() {
        let mut app = test_app();
        add_test_image(&mut app);
        add_test_image(&mut app);
        assert_eq!(app.composer, "[▧ image.png][▧ image.png · 2]");
    }

    #[test]
    fn image_payloads_follow_visual_pill_order() {
        let first = HeldInput::Image {
            label: "first.png".into(),
            image: orca_harness_core::Image {
                media_type: "image/png".into(),
                data: "first".into(),
            },
        };
        let second = HeldInput::Image {
            label: "second.png".into(),
            image: orca_harness_core::Image {
                media_type: "image/png".into(),
                data: "second".into(),
            },
        };
        let held = vec![first, second];
        let prompt = format!("{} then {}", held[1].marker(2), held[0].marker(1));

        let images = prompt_images(&held, &prompt);
        assert_eq!(images[0].data, "second");
        assert_eq!(images[1].data, "first");
    }

    #[test]
    fn a_short_single_line_paste_is_typed_through() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        for c in "run ".chars() {
            press(&mut app, &tx, KeyCode::Char(c));
        }

        paste(&mut app, &tx, "cargo test --all");

        assert_eq!(app.composer, "run cargo test --all");
        assert!(app.pastes.is_empty(), "nothing to hold aside");
    }

    #[test]
    fn crlf_pastes_are_normalized_before_counting() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();

        paste(&mut app, &tx, "one\r\ntwo\r\nthree\r\n");

        assert_eq!(app.composer, "[Pasted text #1, 3 lines]");
        assert!(matches!(&app.pastes[0], HeldInput::Text(text) if text == "one\ntwo\nthree\n"));
    }

    #[test]
    fn markers_number_upward_and_expand_independently() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();

        paste(&mut app, &tx, "first\nblock");
        paste(&mut app, &tx, "second\nblock");
        assert_eq!(
            app.composer,
            "[Pasted text #1, 2 lines][Pasted text #2, 2 lines]"
        );

        submit(&mut app, &tx, 80);
        match rx.try_recv() {
            Ok(WorkerCmd::Run { prompt, .. }) => {
                assert_eq!(prompt, "first\nblocksecond\nblock");
            }
            other => panic!("expected a run, got {:?}", other.is_ok()),
        }
    }

    /// Text that merely looks like a marker is not a marker.
    #[test]
    fn typed_marker_lookalikes_are_left_alone() {
        assert_eq!(
            expand_pastes(&[], "[Pasted text #1, 3 lines]"),
            "[Pasted text #1, 3 lines]"
        );
        assert_eq!(
            expand_pastes(
                &[HeldInput::Text("a\nb".to_string())],
                "[Pasted text #1, 9 lines]",
            ),
            "[Pasted text #1, 9 lines]"
        );
    }

    fn test_app() -> App {
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
        // A transcript taller than any viewport so scrolling has room.
        for i in 0..100 {
            app.transcript.push(Line::from(format!("line {i}")));
        }
        app
    }

    fn mouse(kind: MouseEventKind) -> CtEvent {
        mouse_at(kind, 0)
    }

    fn mouse_at(kind: MouseEventKind, column: u16) -> CtEvent {
        CtEvent::Mouse(MouseEvent {
            kind,
            column,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn mouse_wheel_scrolls_the_transcript_not_history() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.prompt_history.push("previous prompt".into());

        handle_terminal_event(&mut app, mouse(MouseEventKind::ScrollUp), &tx, 80);
        assert!(app.scroll > 0, "wheel up scrolls back");
        assert!(app.composer.is_empty(), "composer untouched by wheel");
        assert_eq!(app.history_pos, None, "history untouched by wheel");

        let scrolled = app.scroll;
        handle_terminal_event(&mut app, mouse(MouseEventKind::ScrollDown), &tx, 80);
        assert!(app.scroll < scrolled, "wheel down scrolls forward");
    }

    fn ctrl(code: char) -> CtEvent {
        CtEvent::Key(KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL))
    }

    /// The last notice or error the app pushed.
    fn last_notice(app: &App) -> String {
        app.pending_history
            .last()
            .map(line_text)
            .unwrap_or_default()
    }

    /// Copy takes the markdown the model actually produced, not the
    /// wrapped and highlighted lines the transcript holds — pasting the
    /// rendered form would carry the transcript's own indentation.
    #[test]
    fn ctrl_y_copies_the_last_answer_verbatim() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.last_answer = Some("# Title\n\nsome **prose**".into());

        handle_terminal_event(&mut app, ctrl('y'), &tx, 80);
        assert_eq!(
            app.clipboard_pending.as_deref(),
            Some("# Title\n\nsome **prose**")
        );
        assert!(last_notice(&app).contains("copied last answer"));
    }

    /// Hitting copy while the model is still typing should yield what is
    /// on screen, not the previous turn's answer under a notice claiming
    /// otherwise — a wrong clipboard is only discovered on paste.
    #[test]
    fn copy_mid_stream_takes_the_partial_answer_and_names_it() {
        let mut app = test_app();
        app.last_answer = Some("the previous turn".into());
        app.text = "half an answ".into();

        copy_command(&mut app, "");
        assert_eq!(app.clipboard_pending.as_deref(), Some("half an answ"));
        assert!(last_notice(&app).contains("answer so far"));

        // Once the stream closes the buffer empties and the answer stands.
        app.text.clear();
        app.clipboard_pending = None;
        copy_command(&mut app, "");
        assert_eq!(app.clipboard_pending.as_deref(), Some("the previous turn"));
        assert!(last_notice(&app).contains("copied last answer"));
    }

    #[test]
    fn copy_code_takes_the_last_fenced_block() {
        let mut app = test_app();
        app.last_answer = Some("try:\n```sh\ncargo test\n```\nthen ship".into());

        copy_command(&mut app, "code");
        assert_eq!(app.clipboard_pending.as_deref(), Some("cargo test"));
    }

    #[test]
    fn copy_all_flattens_the_transcript_to_plain_text() {
        let mut app = test_app();
        app.transcript.clear();
        app.transcript.push(Line::from(vec![
            Span::styled("• ", Style::default()),
            Span::styled("hello", Style::default()),
        ]));
        // Block spacing leaves trailing blanks that nobody wants pasted.
        app.transcript.push(Line::from(""));
        app.transcript.push(Line::from(""));

        copy_command(&mut app, "all");
        assert_eq!(app.clipboard_pending.as_deref(), Some("• hello"));
    }

    /// A copy that quietly does nothing is worse than one that refuses:
    /// the user pastes whatever was in the clipboard before and does not
    /// notice until it matters.
    #[test]
    fn copy_says_so_when_there_is_nothing_to_copy() {
        let mut app = test_app();
        app.last_answer = None;

        copy_command(&mut app, "");
        assert!(app.clipboard_pending.is_none());
        assert!(last_notice(&app).contains("nothing to copy"));

        app.last_answer = Some("prose with no code in it".into());
        copy_command(&mut app, "code");
        assert!(app.clipboard_pending.is_none());
        assert!(last_notice(&app).contains("last code block"));
    }

    /// Terminals truncate oversized OSC 52 payloads, and a half-copied
    /// answer pastes without any sign that it was cut.
    #[test]
    fn copy_refuses_a_payload_the_terminal_would_truncate() {
        let mut app = test_app();
        app.last_answer = Some("x".repeat(clipboard::MAX_COPY_BYTES + 1));

        copy_command(&mut app, "");
        assert!(app.clipboard_pending.is_none());
        assert!(last_notice(&app).contains("clipboard write"));
    }

    #[test]
    fn copy_rejects_an_unknown_target() {
        let mut app = test_app();
        app.last_answer = Some("something".into());

        copy_command(&mut app, "everything");
        assert!(app.clipboard_pending.is_none());
        assert!(last_notice(&app).contains("unknown /copy target"));
    }

    #[test]
    fn clearing_the_conversation_drops_the_copyable_answer() {
        let mut app = test_app();
        app.last_answer = Some("gone after /clear".into());

        reset_conversation_ui(&mut app);
        assert!(app.last_answer.is_none());
    }

    /// The palette shows a window onto the command list, so navigating
    /// past the last visible row has to slide it. Arrow keys, page keys,
    /// and the wheel all drive the same selection; before this, only the
    /// arrows did, and the wheel and page keys scrolled the transcript
    /// behind the open palette instead.
    #[test]
    fn palette_scrolls_its_own_list_by_arrows_page_keys_and_wheel() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "/".into();
        app.cursor = 1;
        let total = filter_commands("").len();
        assert!(
            total > PALETTE_ROWS,
            "this test needs more commands than fit: {total}"
        );

        // The window starts at the top and stays there while the
        // selection is inside it.
        let window = |app: &App| flat_lines(&palette_lines(app, PALETTE_ROWS + 2, 100));
        assert!(window(&app).contains(&format!("1-{PALETTE_ROWS}")));

        // One step past the last visible row slides the window by one.
        for _ in 0..PALETTE_ROWS {
            handle_terminal_event(
                &mut app,
                CtEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                &tx,
                80,
            );
        }
        assert_eq!(app.palette_index, PALETTE_ROWS);
        assert!(
            window(&app).contains(&format!("2-{}", PALETTE_ROWS + 1)),
            "{}",
            window(&app)
        );
        assert_eq!(app.scroll, 0, "the transcript stays put");

        // Page keys move a screenful of the list, not of the transcript.
        handle_terminal_event(
            &mut app,
            CtEvent::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
            &tx,
            80,
        );
        assert_eq!(app.palette_index, 0);
        assert_eq!(app.scroll, 0, "page keys do not reach the transcript");
        handle_terminal_event(
            &mut app,
            CtEvent::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
            &tx,
            80,
        );
        assert_eq!(app.palette_index, PALETTE_ROWS);

        // The wheel does the same, one row at a time, and is clamped.
        handle_terminal_event(&mut app, mouse(MouseEventKind::ScrollUp), &tx, 80);
        assert_eq!(app.palette_index, PALETTE_ROWS - 1);
        assert_eq!(app.scroll, 0, "the wheel does not reach the transcript");
        for _ in 0..total * 2 {
            handle_terminal_event(&mut app, mouse(MouseEventKind::ScrollDown), &tx, 80);
        }
        assert_eq!(app.palette_index, total - 1, "clamped at the last entry");
        for _ in 0..total * 2 {
            handle_terminal_event(&mut app, mouse(MouseEventKind::ScrollUp), &tx, 80);
        }
        assert_eq!(app.palette_index, 0, "clamped at the first entry");
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn mouse_wheel_over_split_inspector_scrolls_only_the_inspector() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.view_mode = ViewMode::Split;
        let transcript_scroll = app.scroll;

        handle_terminal_event(
            &mut app,
            mouse_at(MouseEventKind::ScrollDown, 100),
            &tx,
            120,
        );
        assert_eq!(app.split_scroll, 3);
        assert_eq!(app.scroll, transcript_scroll, "left transcript stays put");

        handle_terminal_event(&mut app, mouse_at(MouseEventKind::ScrollUp, 100), &tx, 120);
        assert_eq!(app.split_scroll, 0);
    }

    #[test]
    fn scrolled_transcript_keeps_the_same_top_row_as_live_height_changes() {
        let mut app = test_app();
        app.transcript_max_scroll = 100;
        app.scroll = 10;
        let original_top = app.transcript_max_scroll - app.scroll;

        stabilize_transcript_scroll(&mut app, 112);
        assert_eq!(app.scroll, 22);
        assert_eq!(112 - app.scroll, original_top);

        stabilize_transcript_scroll(&mut app, 96);
        assert_eq!(app.scroll, 6);
        assert_eq!(96 - app.scroll, original_top);

        app.scroll = 0;
        stabilize_transcript_scroll(&mut app, 140);
        assert_eq!(app.scroll, 0, "bottom-follow mode remains at the bottom");
    }

    #[test]
    fn split_renders_new_transcript_blocks_at_the_left_panes_real_width() {
        let mut app = test_app();
        app.transcript.clear();
        app.view_mode = ViewMode::Split;
        let width = transcript_content_width(&app, 120);
        assert_eq!(width, 69);

        app.push_markdown_block(
            "| Component | Responsibility | Notes |\n|---|---|---|\n| harness-core | Dispatch and concurrency | deterministic ordered results |",
            width,
            BlockSpacing::Section,
        );
        assert!(
            app.pending_history
                .iter()
                .all(|line| line_text(line).chars().count() <= width),
            "markdown is laid out for the pane before it is committed"
        );
    }

    #[test]
    fn inspector_caps_large_file_previews() {
        let output = serde_json::Value::String(
            (0..400)
                .map(|line| format!("line {line}: {}", "x".repeat(200)))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let (preview, omitted) = inspector_output_preview("read_file", &output, None, false);
        assert!(omitted);
        assert!(preview.len() <= INSPECTOR_PREVIEW_CHARS);
        assert!(preview.lines().count() <= INSPECTOR_PREVIEW_LINES);
    }

    #[test]
    fn inspector_shell_output_leads_with_signal_and_keeps_the_tail() {
        let stdout = (0..40)
            .map(|line| format!("build line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = serde_json::json!({
            "stdout": stdout,
            "stderr": "",
            "exitCode": 0,
            "success": true
        });
        let (preview, omitted) = inspector_output_preview("shell", &output, None, false);
        assert!(preview.starts_with("exit 0 · 40 stdout\n"));
        assert!(preview.contains("build line 0"));
        assert!(preview.contains("build line 39"));
        assert!(!preview.contains("build line 20"));
        assert!(omitted);
    }

    #[test]
    fn inspector_collection_output_shows_count_without_json_scaffolding() {
        let output = serde_json::json!({
            "query": "ToolActivity",
            "matches": ["src/tui.rs:181", "src/tui.rs:1861"],
            "truncated": false
        });
        let (preview, omitted) = inspector_output_preview("grep", &output, Some("json"), false);
        assert_eq!(preview, "2 matches\nsrc/tui.rs:181\nsrc/tui.rs:1861");
        assert!(!omitted);
    }

    #[test]
    fn inspector_classifies_results_by_shape_not_tool_name() {
        let execution = serde_json::json!({
            "stdout": "compiled",
            "stderr": "",
            "exitCode": 0
        });
        let (preview, _) = inspector_output_preview("custom_runner", &execution, None, false);
        assert_eq!(preview, "exit 0 · 1 stdout\ncompiled");

        let mutation = serde_json::json!({"path": "src/lib.rs", "bytesWritten": 2048});
        let (preview, _) = inspector_output_preview("custom_writer", &mutation, None, false);
        assert_eq!(preview, "src/lib.rs · wrote 2.0 KiB");
    }

    #[test]
    fn inspector_json_source_renders_content_instead_of_its_envelope() {
        let output = serde_json::json!({
            "content": "{\n  \"name\": \"orca\"\n}",
            "bytes": 20,
            "truncated": false
        });
        let (preview, omitted) = inspector_output_preview("anything", &output, Some("json"), false);
        assert_eq!(preview, "{\n  \"name\": \"orca\"\n}");
        assert!(!omitted);
    }

    #[test]
    fn inspector_source_output_reports_language_shape_and_size() {
        let tool = ToolActivity {
            call_id: "read-1".into(),
            call_line: "read_file src/main.rs".into(),
            tool_name: "read_file".into(),
            input: serde_json::json!({"path": "src/main.rs"}),
            started: Instant::now(),
            elapsed: Some(Duration::from_millis(1)),
            output: None,
            is_error: false,
            approval: None,
        };
        let output = serde_json::json!({
            "content": "fn main() {\n    println!(\"orca\");\n}\n",
            "bytes": 2048,
            "truncated": false
        });
        assert_eq!(
            inspector_code_facts(&tool, &output, Some("rust")).as_deref(),
            Some("rust · 3 lines · 2.0 KiB")
        );
    }

