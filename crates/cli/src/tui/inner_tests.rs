mod tests {
    use super::*;
    use orca_harness_model_openrouter::ModelInfo;
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
        assert_eq!(app.pastes[0], "one\ntwo\nthree\n");
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
            expand_pastes(&["a\nb".to_string()], "[Pasted text #1, 9 lines]"),
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

        let joined = flat_lines(&activity_lines_selected(&app, 180, true, Some(0)));
        assert!(
            joined.contains("shell $ cargo test --workspace · "),
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
            joined.contains("× shell $ cargo test · exit 1"),
            "failure state shown: {joined}"
        );
        assert!(
            joined.contains("test result: FAILED"),
            "stdout expanded: {joined}"
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
        handle_ui_msg(
            &mut app,
            UiMsg::RunDone(Err("model endpoint unavailable".into())),
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

    #[test]
    fn bang_composer_keeps_the_original_unfilled_style() {
        let mut app = test_app();
        app.composer = "!echo hello".into();
        app.cursor = app.composer.chars().count();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 22)].bg, ratatui::style::Color::Reset);
        assert_ne!(buffer[(0, 20)].symbol(), "┌");
    }

    #[test]
    fn empty_session_has_a_useful_static_welcome() {
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
        let screen = rendered_rows(&mut app, 90, 30).join("\n");

        assert!(screen.contains("▀▄ ORCACODE"), "logo missing: {screen}");
        assert!(
            screen.contains(concat!("v", env!("CARGO_PKG_VERSION"))),
            "version missing: {screen}"
        );
        assert!(screen.contains("gpt-oss:20b"), "model missing: {screen}");
        assert!(
            screen.contains("/workspace/orca-harness"),
            "workspace missing: {screen}"
        );
        assert!(
            screen.contains("Describe a task to begin"),
            "welcome hint missing: {screen}"
        );
        assert!(screen.contains("/models switch model"));
    }

    #[test]
    fn welcome_centres_the_visible_card_not_its_maximum_width() {
        let mut app = test_app();
        let rows = rendered_rows(&mut app, 90, 30);
        let subtitle = rows
            .iter()
            .find(|row| row.contains("A small, fast agent runtime"))
            .expect("welcome subtitle");
        let visible_width = "A small, fast agent runtime for your terminal"
            .chars()
            .count();

        assert_eq!(
            subtitle.chars().take_while(|ch| *ch == ' ').count(),
            (90 - visible_width) / 2,
            "the longest visible row defines the card centre: {subtitle:?}"
        );
    }

    #[test]
    fn startup_notices_stay_behind_the_welcome_until_the_first_turn() {
        let mut app = test_app();
        push_notice(&mut app, "MCP docs connected · 4 tools");
        app.absorb_pending();

        let screen = rendered_rows(&mut app, 90, 30).join("\n");

        assert!(screen.contains("▀▄ ORCACODE"), "welcome missing: {screen}");
        assert!(
            !screen.contains("MCP docs connected"),
            "startup notice should remain in the background: {screen}"
        );
        assert!(
            flat_lines(&app.transcript).contains("MCP docs connected"),
            "startup notice should remain recorded"
        );
    }

    #[test]
    fn help_as_the_first_command_replaces_the_welcome_immediately() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();

        slash_command(&mut app, "help", &tx, 90);
        app.absorb_pending();
        let screen = rendered_rows(&mut app, 90, 40).join("\n");

        assert!(
            screen.contains("/help        show this help"),
            "help missing: {screen}"
        );
        assert!(
            !screen.contains("▀▄ ORCACODE"),
            "welcome remained: {screen}"
        );
        assert_eq!(app.turn_count, 0, "local help is not a model turn");
    }

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
    fn status_line_starts_with_model_and_ends_with_workspace_name() {
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

        let rows = rendered_rows(&mut app, 100, 24);
        let status = rows
            .iter()
            .find(|row| row.contains("idle"))
            .expect("status line");

        assert!(
            status.starts_with(" gpt-oss:20b ·"),
            "model not first: {status}"
        );
        assert!(
            status.ends_with("· orca-harness"),
            "workspace not last: {status}"
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

    #[test]
    fn assistant_deltas_render_in_the_transcript_above_live_status() {
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
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        handle_harness_event(
            &mut app,
            HarnessEvent::AssistantDelta {
                text: "streamed answer".into(),
            },
            80,
        );

        let rows = rendered_rows(&mut app, 80, 24);
        let answer_row = rows
            .iter()
            .position(|row| row.contains("streamed answer"))
            .expect("streamed answer rendered");
        let status_row = rows
            .iter()
            .position(|row| row.contains("writing"))
            .expect("live status rendered");
        assert!(
            answer_row < status_row,
            "answer belongs to transcript above live status: {rows:#?}"
        );
    }

    #[test]
    fn composer_has_one_blank_row_above_it_without_a_divider() {
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
        app.transcript.push(Line::from("final answer"));

        let rows = rendered_rows(&mut app, 80, 10);
        let composer = rows
            .iter()
            .position(|row| row.contains("ask anything"))
            .expect("composer");
        assert!(rows[composer - 1].is_empty(), "gap has no divider");
    }

    #[test]
    fn assistant_deltas_are_markdown_rendered_while_streaming() {
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
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        handle_harness_event(
            &mut app,
            HarnessEvent::AssistantDelta {
                text: "# Streaming heading".into(),
            },
            80,
        );

        let joined = rendered_rows(&mut app, 80, 24).join("\n");
        assert!(joined.contains("Streaming heading"));
        assert!(
            !joined.contains("# Streaming heading"),
            "markdown syntax is rendered, not printed raw: {joined}"
        );
    }

    #[test]
    fn completed_assistant_event_does_not_blank_the_stream_before_result() {
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
        app.run = RunState::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
        };
        handle_harness_event(
            &mut app,
            HarnessEvent::AssistantDelta {
                text: "continuous answer".into(),
            },
            80,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::Assistant {
                message: "continuous answer".into(),
            },
            80,
        );

        let joined = rendered_rows(&mut app, 80, 24).join("\n");
        assert!(
            joined.contains("continuous answer"),
            "completed event stays projected until result: {joined}"
        );
    }

    fn press(app: &mut App, tx: &mpsc::UnboundedSender<WorkerCmd>, code: KeyCode) {
        handle_terminal_event(
            app,
            CtEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            tx,
            80,
        );
    }

    fn catalog() -> Vec<ModelInfo> {
        ["acme/fast-1", "acme/smart-9", "other/tiny"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                name: None,
                context_length: Some(32_000),
                pricing: None,
            })
            .collect()
    }

    #[test]
    fn at_opens_the_standard_location_picker_and_filters_as_you_type() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        press(&mut app, &tx, KeyCode::Char('@'));
        let Some(Overlay::Locations(location)) = app.overlay.as_mut() else {
            panic!("expected @ to open the location picker");
        };
        location.entries = vec![
            LocationEntry {
                path: "crates/cli/src/tui.rs".into(),
                directory: false,
            },
            LocationEntry {
                path: "README.md".into(),
                directory: false,
            },
        ];
        location.sync_len();

        press(&mut app, &tx, KeyCode::Char('t'));

        let Some(Overlay::Locations(location)) = &app.overlay else {
            panic!("location picker should remain open");
        };
        assert_eq!(app.composer, "@t");
        assert_eq!(location.query, "t");
        assert_eq!(location.filtered().len(), 1);
    }

    #[test]
    fn backspace_cancels_an_empty_location_picker_and_removes_the_at() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        press(&mut app, &tx, KeyCode::Char('@'));

        press(&mut app, &tx, KeyCode::Backspace);

        assert!(app.overlay.is_none());
        assert_eq!(app.composer, "");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn delete_cancels_an_empty_location_picker_and_removes_the_at() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        press(&mut app, &tx, KeyCode::Char('@'));

        press(&mut app, &tx, KeyCode::Delete);

        assert!(app.overlay.is_none());
        assert_eq!(app.composer, "");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn tab_inserts_the_selected_folder_mention() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "work in @cr".into();
        app.cursor = app.composer.chars().count();
        app.overlay = Some(Overlay::Locations(LocationPicker {
            entries: vec![LocationEntry {
                path: "crates/cli".into(),
                directory: true,
            }],
            query: "cr".into(),
            token_start: 8,
            picker: ListPicker::new(1),
        }));

        press(&mut app, &tx, KeyCode::Tab);

        assert!(app.overlay.is_none());
        assert_eq!(app.composer, "work in @crates/cli/ ");
        assert_eq!(app.cursor, app.composer.chars().count());
    }

    #[test]
    fn backspace_removes_an_inserted_location_mention_in_one_go() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "work in @crates/cli/ ".into();
        app.cursor = app.composer.chars().count();

        press(&mut app, &tx, KeyCode::Backspace);

        assert_eq!(app.composer, "work in ");
        assert_eq!(app.cursor, app.composer.chars().count());
    }

    #[test]
    fn submitting_a_mention_sends_a_plain_path_but_shows_the_at() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "read @docs/crate-diagram.md please".into();
        app.cursor = app.composer.chars().count();

        submit(&mut app, &tx, 80);

        match rx.try_recv() {
            Ok(WorkerCmd::Run { prompt, .. }) => {
                assert_eq!(prompt, "read docs/crate-diagram.md please");
            }
            other => panic!("expected a run, got {:?}", other.is_ok()),
        }
        assert_eq!(
            app.prompt_history.last().map(String::as_str),
            Some("read @docs/crate-diagram.md please"),
            "recall keeps what the user typed"
        );
    }

    #[test]
    fn addresses_and_bare_at_signs_survive_submission() {
        assert_eq!(
            strip_location_mentions("mail dev@example.com about @user@host and @ 5pm"),
            "mail dev@example.com about @user@host and @ 5pm"
        );
    }

    #[test]
    fn at_inside_a_word_does_not_open_the_location_picker() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.composer = "user".into();
        app.cursor = 4;

        press(&mut app, &tx, KeyCode::Char('@'));

        assert!(app.overlay.is_none());
        assert_eq!(app.composer, "user@");
    }

    #[test]
    fn catalog_reply_opens_the_picker_seeded_with_the_command_filter() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.picker_pending = Some("acme".into());
        handle_ui_msg(&mut app, UiMsg::Models(Ok(catalog())), &tx, 80);
        let Some(Overlay::Models(picker)) = &app.overlay else {
            panic!("expected the model picker to open");
        };
        assert_eq!(picker.filter, "acme");
        assert_eq!(picker.filtered().len(), 2);
    }

    #[test]
    fn picker_filters_navigates_and_switches_on_enter() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.overlay = Some(Overlay::Models(ModelPicker {
            models: catalog(),
            filter: String::new(),
            index: 0,
        }));

        // Typing narrows to the two acme models; Down selects the second.
        for c in "acme".chars() {
            press(&mut app, &tx, KeyCode::Char(c));
        }
        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Enter);

        assert!(app.overlay.is_none(), "picker closes on selection");
        match rx.try_recv() {
            Ok(WorkerCmd::SetModel { id }) => assert_eq!(id, "acme/smart-9"),
            other => panic!("expected SetModel, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn picker_escape_closes_without_switching() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.overlay = Some(Overlay::Models(ModelPicker {
            models: catalog(),
            filter: String::new(),
            index: 0,
        }));
        press(&mut app, &tx, KeyCode::Esc);
        assert!(app.overlay.is_none());
        assert!(rx.try_recv().is_err(), "no command sent on cancel");
    }

    #[test]
    fn provider_without_key_requirement_switches_directly() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.overlay = Some(Overlay::Providers {
            picker: ListPicker::new(Provider::ALL.len()),
        });
        // Down twice: openrouter -> openai -> local (needs no key).
        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        match rx.try_recv() {
            Ok(WorkerCmd::SetProvider { provider, api_key }) => {
                assert_eq!(provider, Provider::Local);
                assert!(api_key.is_none());
            }
            other => panic!("expected SetProvider, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn api_key_prompt_masks_input_and_submits_on_enter() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.overlay = Some(Overlay::ApiKey {
            provider: Provider::OpenRouter,
            input: String::new(),
        });

        // Empty enter is ignored — no accidental keyless switch.
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_some());
        assert!(rx.try_recv().is_err());

        for c in "sk-or-abc".chars() {
            press(&mut app, &tx, KeyCode::Char(c));
        }
        // The rendered prompt shows bullets, never the key itself.
        let lines = flat_lines(&live_lines(&app, 80));
        assert!(!lines.contains("sk-or-abc"), "key must be masked: {lines}");
        assert!(lines.contains("•••••••••"));

        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        match rx.try_recv() {
            Ok(WorkerCmd::SetProvider { provider, api_key }) => {
                assert_eq!(provider, Provider::OpenRouter);
                assert_eq!(api_key.as_deref(), Some("sk-or-abc"));
            }
            other => panic!("expected SetProvider, got {:?}", other.is_ok()),
        }
        // The key survives to the next session via the config file.
        assert_eq!(
            crate::config::stored_key("openrouter").as_deref(),
            Some("sk-or-abc")
        );
    }

    #[test]
    fn slash_provider_opens_the_provider_overlay() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        slash_command(&mut app, "provider", &tx, 80);
        match &app.overlay {
            Some(Overlay::Providers { picker }) => assert_eq!(picker.index(), 0),
            _ => panic!("expected the provider overlay"),
        }
    }

    #[test]
    fn settings_menu_drills_into_the_matching_pickers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        slash_command(&mut app, "settings", &tx, 80);
        match &app.overlay {
            Some(Overlay::Settings { picker }) => assert_eq!(picker.index(), 0),
            _ => panic!("expected the settings overlay"),
        }

        // Provider row: opens the provider picker preselected on the
        // active provider (local sits at index 2).
        press(&mut app, &tx, KeyCode::Enter);
        match &app.overlay {
            Some(Overlay::Providers { picker }) => assert_eq!(picker.index(), 2),
            _ => panic!("expected the provider overlay"),
        }

        // Model row: kicks off the same fetch-then-pick flow as /models.
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 1),
        });
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ListModels { .. })));
        assert_eq!(app.picker_pending.as_deref(), Some(""));

        // Theme row: opens the theme picker.
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 2),
        });
        press(&mut app, &tx, KeyCode::Enter);
        assert!(matches!(app.overlay, Some(Overlay::Themes { .. })));

        // View row opens a picker preselected on the current layout.
        app.view_mode = ViewMode::Classic;
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 3),
        });
        press(&mut app, &tx, KeyCode::Enter);
        match &app.overlay {
            Some(Overlay::Views { picker }) => assert_eq!(picker.index(), 0),
            _ => panic!("expected the view overlay"),
        }

        // Down and enter selects Split using the same pattern as theme/provider.
        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        assert!(app.view_mode == ViewMode::Split);
        assert_eq!(crate::config::stored_view().as_deref(), Some("split"));

        // Api key row on a keyless provider closes with an explanation.
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 4),
        });
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        assert!(rx.try_recv().is_err(), "no command for a keyless provider");

        // Transcript spacing is a persisted picker and applies immediately.
        // The preference is process-global and seeded by every `App::new`,
        // so hold the guard while we transition it live.
        let _spacing = SPACING_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        set_transcript_spacing(TranscriptSpacing::Comfortable);
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 6),
        });
        press(&mut app, &tx, KeyCode::Enter);
        match &app.overlay {
            Some(Overlay::TranscriptSpacing { picker }) => assert_eq!(picker.index(), 1),
            _ => panic!("expected transcript spacing overlay"),
        }
        press(&mut app, &tx, KeyCode::Up);
        press(&mut app, &tx, KeyCode::Enter);
        assert_eq!(transcript_spacing(), TranscriptSpacing::Compact);
        assert_eq!(
            crate::config::stored_transcript_spacing().as_deref(),
            Some("compact")
        );
        set_transcript_spacing(TranscriptSpacing::Comfortable);
        let _ = crate::config::save_transcript_spacing("comfortable");
    }

    #[test]
    fn split_view_connects_the_selected_tool_to_its_inspector() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.view_mode = ViewMode::Split;
        let empty = rendered_rows(&mut app, 120, 24).join("\n");
        assert!(
            empty.contains("TOOL INSPECTOR"),
            "split geometry exists before calls: {empty}"
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "call-1".into(),
                tool_name: "shell".into(),
                input: serde_json::json!({
                    "command": format!("cargo test {}", "heterogeneous_burst_".repeat(6))
                }),
            },
            120,
        );

        let rail = flat_lines(&activity_lines_selected(&app, 70, true, Some(0)));
        assert!(
            rail.contains("·"),
            "selected row has a dotted leader: {rail}"
        );
        assert!(
            rail.contains('○'),
            "selected row ends at a connection node: {rail}"
        );
        let connected = activity_lines_selected(&app, 70, true, Some(0))
            .into_iter()
            .map(|line| line_text(&line))
            .find(|line| line.contains('○'))
            .expect("connector row");
        assert!(
            connected.contains("shell") && connected.chars().count() <= 70,
            "call and connector stay on one row: {connected}"
        );

        let inspector = flat_lines(&tool_inspector_lines(&app.activity_tools[0], 50));
        assert!(
            inspector.contains("SHELL") && inspector.contains("running"),
            "tool identity repeats: {inspector}"
        );
        assert!(
            inspector.contains("Run a shell command"),
            "tool action is explained: {inspector}"
        );
        assert!(
            inspector.contains("cargo test"),
            "input is expanded: {inspector}"
        );
        assert!(inspector.contains("waiting for result"));

        handle_terminal_event(
            &mut app,
            CtEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &tx,
            120,
        );
        assert!(app.split_focused);
        press(&mut app, &tx, KeyCode::Esc);
        assert!(!app.split_focused);

        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "call-1".into(),
                tool_name: "shell".into(),
                output: serde_json::json!({
                    "stdout": (0..50).map(|line| format!("result {line}")).collect::<Vec<_>>().join("\n"),
                    "exit_code": 0
                }),
                is_error: false,
            },
            120,
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::Assistant {
                message: "done".into(),
            },
            120,
        );
        assert!(app.activity_tools.is_empty(), "phase was committed");
        assert!(
            flat_lines(&app.pending_history).contains('○'),
            "the last committed call keeps its connector"
        );
        app.split_scroll = 10;
        let settled = rendered_rows(&mut app, 120, 24).join("\n");
        assert!(
            settled.contains("SHELL") && settled.contains("result"),
            "the pane stays mounted and its header stays pinned: {settled}"
        );
    }

    #[test]
    fn split_divider_runs_through_composer_and_status_rows() {
        let mut app = test_app();
        app.view_mode = ViewMode::Split;
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let inspector_x =
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                .split(ratatui::layout::Rect::new(0, 0, 120, 24))[1]
                .x;
        let buffer = terminal.backend().buffer();
        for y in 0..24 {
            assert_eq!(
                buffer[(inspector_x, y)].symbol(),
                "│",
                "divider missing at row {y}"
            );
        }
    }

    #[test]
    fn capital_a_saves_the_approval_and_settings_can_revoke_it() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();

        // With nothing saved, the settings approvals row just explains.
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 5),
        });
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());

        // Capital A persists the tool for this workspace.
        let (respond, mut answer) = tokio::sync::oneshot::channel();
        app.approval = Some(crate::msg::ApprovalRequest {
            tool_name: "shell".into(),
            detail: "shell $ ls".into(),
            respond,
        });
        handle_approval_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
        );
        assert_eq!(
            answer.try_recv().unwrap(),
            ApprovalResponse::AllowAlwaysSave
        );
        assert_eq!(crate::config::stored_approvals("/test-ws"), ["shell"]);

        // Lowercase a stays session-only: nothing new is persisted.
        let (respond, mut answer) = tokio::sync::oneshot::channel();
        app.approval = Some(crate::msg::ApprovalRequest {
            tool_name: "write_file".into(),
            detail: "write_file x".into(),
            respond,
        });
        handle_approval_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert_eq!(answer.try_recv().unwrap(), ApprovalResponse::AllowAlways);
        assert_eq!(crate::config::stored_approvals("/test-ws"), ["shell"]);

        // The settings approvals row opens the list; enter revokes.
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 5),
        });
        press(&mut app, &tx, KeyCode::Enter);
        assert!(matches!(app.overlay, Some(Overlay::Approvals { .. })));
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none(), "removing the last entry closes");
        assert!(crate::config::stored_approvals("/test-ws").is_empty());
    }

    #[test]
    fn slash_theme_opens_a_picker_preselected_on_the_active_theme() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        slash_command(&mut app, "theme", &tx, 80);
        let current = view::theme_name();
        let expected = view::ThemeName::ALL
            .iter()
            .position(|name| *name == current)
            .unwrap();
        match &app.overlay {
            Some(Overlay::Themes { picker }) => assert_eq!(picker.index(), expected),
            other => panic!("expected theme overlay, got {}", other.is_some()),
        }

        let listing = flat_lines(&live_lines(&app, 100));
        assert!(listing.contains("Select theme"), "{listing}");
        assert!(listing.contains("Dracula"), "{listing}");
        assert!(listing.contains("current"), "{listing}");

        // Down then up returns to the active theme; enter re-applies it,
        // closes the overlay, and notes the choice. No worker involved.
        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Up);
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        assert!(rx.try_recv().is_err(), "theme switching is UI-local");
        assert_eq!(view::theme_name(), current);
        let notes = app
            .pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(notes.contains("theme set to"), "{notes}");
    }
}

#[cfg(test)]
mod theme_command_tests {
    use super::*;

    fn theme_app() -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
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
        })
    }

    /// One test, single-threaded, because the theme is process-global: it
    /// must not interleave with any other mutating test. Ends by restoring
    /// the default so later tests see a clean state.
    #[test]
    fn theme_command_switches_and_rejects() {
        let _theme = THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let mut app = theme_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        for (cmd, expected) in [
            ("theme dracula", view::ThemeName::Dracula),
            ("theme solarized-dark", view::ThemeName::SolarizedDark),
            ("theme one-dark", view::ThemeName::OneDark),
            ("theme monokai", view::ThemeName::Monokai),
            ("theme nord", view::ThemeName::Nord),
            ("theme default", view::ThemeName::Default),
            ("theme mono", view::ThemeName::Mono),
        ] {
            slash_command(&mut app, cmd, &worker, 80);
            assert_eq!(view::theme_name(), expected, "command {cmd}");
        }

        // Unknown names are rejected and leave the theme unchanged.
        slash_command(&mut app, "theme midnight", &worker, 80);
        assert_eq!(
            view::theme_name(),
            view::ThemeName::Mono,
            "unchanged on garbage"
        );

        // Bare form only reports; it must not change the value.
        slash_command(&mut app, "theme", &worker, 80);
        assert_eq!(
            view::theme_name(),
            view::ThemeName::Mono,
            "bare form does not change"
        );
        let text = app
            .pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Mono"), "reports current name: {text}");

        // Restore the default so other tests (and the view) see clean state.
        slash_command(&mut app, "theme color", &worker, 80); // legacy alias
        assert_eq!(
            view::theme_name(),
            view::ThemeName::Default,
            "color stays a legacy alias for default"
        );
    }
}

#[cfg(test)]
mod subagents_command_tests {
    use super::*;
    use orca_harness_tools::SubagentDepth;

    fn depth_app(depth: SubagentDepth) -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: depth,
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        })
    }

    #[tokio::test]
    async fn subagents_command_sets_and_clamps_depth() {
        let depth = SubagentDepth::new(1);
        let mut app = depth_app(depth.clone());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "subagents 3", &worker, 80);
        assert_eq!(depth.get(), 3);

        slash_command(&mut app, "subagents 99", &worker, 80);
        assert_eq!(depth.get(), 5, "out-of-range input clamps");

        // Bare form only reports; it must not change the value.
        slash_command(&mut app, "subagents", &worker, 80);
        assert_eq!(depth.get(), 5);

        // Garbage input leaves the value alone.
        slash_command(&mut app, "subagents lots", &worker, 80);
        assert_eq!(depth.get(), 5);
    }
}

#[cfg(test)]
mod mode_rewind_todo_tests {
    use super::*;
    use crate::mode::{Mode, ModeHandle};
    use orca_harness_tools::TodoList;

    fn app_with(mode: ModeHandle, todos: TodoList) -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode,
            todos,
            plan: Default::default(),
        })
    }

    fn texts(app: &App) -> String {
        app.pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn mode_toggles_bare_and_sets_by_name() {
        let mode = ModeHandle::default();
        let mut app = app_with(mode.clone(), TodoList::new());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "mode", &worker, 80);
        assert_eq!(mode.get(), Mode::Plan, "bare /mode toggles");
        slash_command(&mut app, "mode", &worker, 80);
        assert_eq!(mode.get(), Mode::Normal);

        slash_command(&mut app, "mode plan", &worker, 80);
        assert_eq!(mode.get(), Mode::Plan);
        // Setting the mode it is already in is not a toggle.
        slash_command(&mut app, "mode plan", &worker, 80);
        assert_eq!(mode.get(), Mode::Plan);
        slash_command(&mut app, "mode normal", &worker, 80);
        assert_eq!(mode.get(), Mode::Normal);

        // Garbage leaves the mode alone and says so.
        slash_command(&mut app, "mode sideways", &worker, 80);
        assert_eq!(mode.get(), Mode::Normal);
        // Glyphed like every other system line, not flush-left.
        assert!(texts(&app).contains("• unknown mode: sideways"));
    }

    /// Leaving plan mode reports the plans that were actually written,
    /// and stays quiet when the agent decided none was warranted —
    /// looking around in plan mode is a legitimate use of it.
    #[tokio::test]
    async fn leaving_plan_mode_reports_only_plans_that_were_written() {
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        // An episode where the agent judged no plan was needed: silence.
        let mut app = app_with(ModeHandle::new(Mode::Plan), TodoList::new());
        slash_command(&mut app, "mode normal", &worker, 80);
        let rendered = texts(&app);
        assert!(rendered.contains("normal mode"), "{rendered}");
        assert!(!rendered.contains("plan saved"), "{rendered}");
        assert!(!rendered.contains("no plan"), "{rendered}");

        // An episode where it wrote two.
        let mut app = app_with(ModeHandle::new(Mode::Plan), TodoList::new());
        app.cfg.plan.record("docs/plan/2026-08-22-first.md");
        app.cfg.plan.record("docs/plan/2026-08-22-second.md");
        slash_command(&mut app, "mode normal", &worker, 80);
        let rendered = texts(&app);
        assert!(
            rendered.contains("plan saved to docs/plan/2026-08-22-first.md"),
            "{rendered}"
        );
        assert!(
            rendered.contains("plan saved to docs/plan/2026-08-22-second.md"),
            "{rendered}"
        );
        assert!(app.cfg.plan.written().is_empty(), "the episode ended");
    }

    /// Entering plan mode must not claim anything about files — at that
    /// point nobody knows whether the conversation warrants one.
    #[tokio::test]
    async fn entering_plan_mode_says_nothing_about_files() {
        let mut app = app_with(ModeHandle::new(Mode::Normal), TodoList::new());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        slash_command(&mut app, "mode plan", &worker, 80);
        let rendered = texts(&app);
        assert!(rendered.contains("plan mode"), "{rendered}");
        assert!(!rendered.contains("plan saved"), "{rendered}");
        assert!(!rendered.contains("docs/plan"), "{rendered}");
    }

    /// Plan mode is a restriction the user must not be able to lose
    /// track of, so it is on the status line while it is on and absent
    /// when it is not.
    #[test]
    fn plan_mode_shows_in_the_status_line() {
        let mode = ModeHandle::default();
        let plan = crate::plan::PlanArea::new();
        assert_eq!(mode_segment(&mode, &plan), "");
        mode.set(Mode::Plan);
        assert_eq!(mode_segment(&mode, &plan), " · plan mode");
        // A landed plan is visible without waiting for /mode normal.
        plan.record("docs/plan/2026-08-22-a.md");
        assert_eq!(mode_segment(&mode, &plan), " · plan mode · 1 plan");
        plan.record("docs/plan/2026-08-22-b.md");
        assert_eq!(mode_segment(&mode, &plan), " · plan mode · 2 plans");
        // Normal mode says nothing, whatever was written.
        mode.set(Mode::Normal);
        assert_eq!(mode_segment(&mode, &plan), "");
    }

    /// Write a task list through the real tool, the way the model does.
    async fn set_todos(todos: &TodoList, items: serde_json::Value) {
        let tool = orca_harness_tools::TodoWriteTool::new(todos.clone());
        let ctx = orca_harness_core::ToolContext {
            call_id: "c".into(),
            tool_name: "todo_write".into(),
            cancellation: orca_harness_core::CancellationToken::new(),
            deadline: None,
        };
        orca_harness_core::Tool::call(&tool, serde_json::json!({ "todos": items }), &ctx)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn todo_progress_shows_in_the_status_line_once_there_is_a_list() {
        let todos = TodoList::new();
        assert_eq!(todo_segment(&todos), "", "silent with no list");
        set_todos(
            &todos,
            serde_json::json!([
                {"content": "a", "status": "completed"},
                {"content": "b", "status": "in_progress"},
                {"content": "c"}
            ]),
        )
        .await;
        assert_eq!(todo_segment(&todos), " · todo 1/3");
    }

    #[tokio::test]
    async fn todo_progress_pins_the_full_plan_in_the_live_region() {
        let todos = TodoList::new();
        set_todos(
            &todos,
            serde_json::json!([
                {"content": "inspect the rendering", "status": "completed"},
                {"content": "add a visible progress cue", "status": "in_progress"},
                {"content": "verify it"}
            ]),
        )
        .await;
        let app = app_with(ModeHandle::default(), todos);

        let rendered = live_lines(&app, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("todo · 1/3 done"), "{rendered}");
        assert!(rendered.contains("├ ✓ inspect the rendering"), "{rendered}");
        assert!(
            rendered.contains("├ ▸ add a visible progress cue"),
            "{rendered}"
        );
        assert!(rendered.contains("└ □ verify it"), "{rendered}");
    }

    #[tokio::test]
    async fn completed_todo_progress_says_complete() {
        let todos = TodoList::new();
        set_todos(
            &todos,
            serde_json::json!([
                {"content": "inspect", "status": "completed"},
                {"content": "verify", "status": "completed"}
            ]),
        )
        .await;
        let app = app_with(ModeHandle::default(), todos);

        let rendered = live_lines(&app, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("todo · 2/2 done"), "{rendered}");
        assert!(rendered.contains("├ ✓ inspect"), "{rendered}");
        assert!(rendered.contains("└ ✓ verify"), "{rendered}");
    }

    #[tokio::test]
    async fn todo_renders_the_list_and_says_so_when_there_is_none() {
        let todos = TodoList::new();
        let mut app = app_with(ModeHandle::default(), todos.clone());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "todo", &worker, 80);
        assert!(texts(&app).contains("no task list"));

        set_todos(
            &todos,
            serde_json::json!([
                {"content": "read the code", "status": "completed"},
                {"content": "write the fix", "status": "in_progress"}
            ]),
        )
        .await;

        slash_command(&mut app, "todo", &worker, 80);
        let rendered = texts(&app);
        assert!(rendered.contains("1/2 done"), "{rendered}");
        assert!(rendered.contains("✓ read the code"), "{rendered}");
        assert!(rendered.contains("▸ write the fix"), "{rendered}");
    }

    #[tokio::test]
    async fn rewind_sends_the_turn_count_and_rejects_nonsense() {
        let mut app = app_with(ModeHandle::default(), TodoList::new());
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "rewind", &worker, 80);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::Rewind { turns: 1 })));

        slash_command(&mut app, "rewind 3", &worker, 80);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::Rewind { turns: 3 })));

        // Zero and garbage send nothing and explain themselves.
        slash_command(&mut app, "rewind 0", &worker, 80);
        slash_command(&mut app, "rewind lots", &worker, 80);
        assert!(rx.try_recv().is_err(), "bad input sends no command");
        assert!(texts(&app).contains("usage: /rewind"));
    }

    #[tokio::test]
    async fn fork_asks_the_worker_to_branch() {
        let mut app = app_with(ModeHandle::default(), TodoList::new());
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();
        slash_command(&mut app, "fork", &worker, 80);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::Fork)));
    }

    /// A rewind redraws the transcript from the shortened context, but
    /// the tokens it already spent are not conversation state.
    #[test]
    fn rewind_redraws_the_transcript_and_keeps_the_token_totals() {
        let mut app = app_with(ModeHandle::default(), TodoList::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        app.tokens_in = 1200;
        app.tokens_out = 340;
        app.usage_steps = 4;
        app.context_tokens = 9000;

        handle_ui_msg(
            &mut app,
            UiMsg::ContextRewound {
                messages: vec![
                    orca_harness_core::Message::System {
                        content: "sys".into(),
                    },
                    orca_harness_core::Message::User {
                        content: "still here".into(),
                    },
                ],
                notice: "rewound 1 turn · 2 messages dropped".into(),
            },
            &tx,
            80,
        );

        let rendered = texts(&app);
        assert!(rendered.contains("rewound 1 turn"), "{rendered}");
        assert!(rendered.contains("still here"), "{rendered}");
        assert_eq!(app.tokens_in, 1200, "spent tokens are not un-spent");
        assert_eq!(app.tokens_out, 340);
        assert_eq!(app.usage_steps, 4);
        assert_eq!(app.turn_count, 1, "turn count follows the new transcript");
        assert_eq!(app.context_tokens, 0, "occupancy waits for the next step");
    }

    #[test]
    fn forking_moves_the_session_id_without_touching_the_transcript() {
        let mut app = app_with(ModeHandle::default(), TodoList::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        app.cfg.session_id = Some("old-id".into());
        app.turn_count = 3;

        handle_ui_msg(
            &mut app,
            UiMsg::SessionForked {
                id: "new-id".into(),
                parent: "old-id".into(),
            },
            &tx,
            80,
        );

        assert_eq!(app.cfg.session_id.as_deref(), Some("new-id"));
        assert_eq!(app.turn_count, 3, "the conversation did not change");
        let rendered = texts(&app);
        assert!(rendered.contains("forked to session new-id"), "{rendered}");
        assert!(rendered.contains("old-id is left as it was"), "{rendered}");
    }
}

#[cfg(test)]
mod extensions_command_tests {
    use super::*;

    fn ext_app() -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
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
        })
    }

    fn printed(app: &App) -> String {
        app.pending_history
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn bare_form_opens_the_picker_and_enter_toggles_in_place() {
        let mut app = ext_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "extensions", &worker, 80);
        match &app.overlay {
            Some(Overlay::Extensions { picker }) => assert_eq!(picker.index(), 0),
            _ => panic!("expected the extensions overlay"),
        }

        // Same rendered shape as the other pickers: every extension with
        // its live state and a selection marker.
        let lines = live_lines(&app, 80)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone().into_owned()))
            .collect::<String>();
        assert!(lines.contains("truncation"), "lists truncation: {lines}");
        assert!(lines.contains("retry"), "lists retry: {lines}");
        assert!(lines.contains("enter toggle"), "shows key hint: {lines}");

        // Enter on the first row (truncation, default on) turns it off,
        // asks the worker to rebuild, and keeps the picker open.
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_overlay_key(&mut app, key, &worker);
        assert_eq!(crate::config::stored_extension("truncation"), Some(false));
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadExtensions)));
        assert!(matches!(app.overlay, Some(Overlay::Extensions { .. })));

        // A second enter toggles it right back on.
        handle_overlay_key(&mut app, key, &worker);
        assert_eq!(crate::config::stored_extension("truncation"), Some(true));
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadExtensions)));

        // Down then enter toggles the second row (retry, default off).
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        handle_overlay_key(&mut app, down, &worker);
        handle_overlay_key(&mut app, key, &worker);
        assert_eq!(crate::config::stored_extension("retry"), Some(true));

        // Esc closes like every other overlay.
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_overlay_key(&mut app, esc, &worker);
        assert!(app.overlay.is_none());
    }

    #[tokio::test]
    async fn typed_form_saves_the_toggle_and_reloads() {
        let mut app = ext_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "extensions enable retry", &worker, 80);
        assert_eq!(crate::config::stored_extension("retry"), Some(true));
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadExtensions)));
        assert!(printed(&app).contains("extension retry enabled"));

        slash_command(&mut app, "extensions disable truncation", &worker, 80);
        assert_eq!(crate::config::stored_extension("truncation"), Some(false));
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadExtensions)));

        // "add" and "delete" are accepted aliases.
        slash_command(&mut app, "extensions delete retry", &worker, 80);
        assert_eq!(crate::config::stored_extension("retry"), Some(false));
        slash_command(&mut app, "extensions add truncation", &worker, 80);
        assert_eq!(crate::config::stored_extension("truncation"), Some(true));
    }

    #[tokio::test]
    async fn bad_input_reports_and_sends_nothing() {
        let mut app = ext_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "extensions enable nope", &worker, 80);
        assert!(printed(&app).contains("unknown extension: nope"));
        assert!(
            printed(&app).contains("truncation, retry"),
            "names the valid set: {}",
            printed(&app)
        );

        slash_command(&mut app, "extensions frobnicate retry", &worker, 80);
        assert!(printed(&app).contains("usage: /extensions"));

        assert!(rx.try_recv().is_err(), "bad input sends nothing");
    }

    fn session_file(id: &str, model: &str) -> orca_harness_extensions::SessionFile {
        orca_harness_extensions::SessionFile {
            path: std::path::PathBuf::from(format!("/tmp/{id}.jsonl")),
            meta: orca_harness_extensions::SessionMeta {
                v: orca_harness_extensions::SESSION_FORMAT_VERSION,
                id: id.into(),
                created_at: 0,
                workspace: "/test-ws".into(),
                model: model.into(),
                parent: None,
            },
        }
    }

    #[tokio::test]
    async fn sessions_picker_navigates_and_enter_resumes() {
        let mut app = ext_app();
        app.cfg.session_id = Some("0000000002-b-0".into());
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // Newest first, preselected on the current session (row 0).
        app.overlay = Some(Overlay::Sessions {
            sessions: vec![
                session_file("0000000002-b-0", "m2"),
                session_file("0000000001-a-0", "m1"),
            ],
            picker: ListPicker::new(2),
        });

        // Same rendered shape as the other pickers: every session with a
        // selection marker, the current one labeled.
        let lines = live_lines(&app, 100)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone().into_owned()))
            .collect::<String>();
        assert!(lines.contains("enter resume"), "shows key hint: {lines}");
        assert!(lines.contains("(current)"), "marks current: {lines}");
        assert!(lines.contains("0000000001-a-0"), "lists both: {lines}");

        // Down then enter resumes the older session and closes the picker.
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_overlay_key(&mut app, down, &worker);
        handle_overlay_key(&mut app, enter, &worker);
        match rx.try_recv() {
            Ok(WorkerCmd::LoadSession { path }) => {
                assert_eq!(path, std::path::PathBuf::from("/tmp/0000000001-a-0.jsonl"));
            }
            other => panic!("expected LoadSession, got {:?}", other.is_ok()),
        }
        assert!(app.overlay.is_none());

        // Esc closes like every other overlay.
        app.overlay = Some(Overlay::Sessions {
            sessions: vec![session_file("0000000001-a-0", "m1")],
            picker: ListPicker::new(1),
        });
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_overlay_key(&mut app, esc, &worker);
        assert!(app.overlay.is_none());
    }

    #[tokio::test]
    async fn space_d_deletes_a_session_but_never_the_active_one() {
        let dir = std::env::temp_dir().join(format!("orca-tui-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let on_disk = |id: &str, model: &str| {
            let mut session = session_file(id, model);
            session.path = dir.join(format!("{id}.jsonl"));
            std::fs::write(&session.path, "{}\n").unwrap();
            session
        };

        let mut app = ext_app();
        app.cfg.session_id = Some("0000000002-b-0".into());
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let active = on_disk("0000000002-b-0", "m2");
        let old = on_disk("0000000001-a-0", "m1");
        let active_path = active.path.clone();
        let old_path = old.path.clone();
        app.overlay = Some(Overlay::Sessions {
            sessions: vec![active, old],
            picker: ListPicker::new(2).actions(SESSION_ACTIONS),
        });

        // Space + d on the active session (row 0): refused, file kept.
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        let d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);
        handle_overlay_key(&mut app, space, &worker);
        handle_overlay_key(&mut app, d, &worker);
        assert!(active_path.exists(), "active session file kept");
        assert!(printed(&app).contains("cannot be deleted"));
        assert!(matches!(app.overlay, Some(Overlay::Sessions { .. })));

        // Down, space + d: the old session is deleted and the list
        // shrinks in place with the picker still open.
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        handle_overlay_key(&mut app, down, &worker);
        handle_overlay_key(&mut app, space, &worker);
        handle_overlay_key(&mut app, d, &worker);
        assert!(!old_path.exists(), "old session file removed");
        assert!(printed(&app).contains("deleted session 0000000001-a-0"));
        match &app.overlay {
            Some(Overlay::Sessions { sessions, picker }) => {
                assert_eq!(sessions.len(), 1);
                assert_eq!(picker.index(), 0);
            }
            _ => panic!("picker stays open while rows remain"),
        }
        assert!(rx.try_recv().is_err(), "deleting sends nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sessions_picker_windows_to_the_last_few_and_pages_like_models() {
        let mut app = ext_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        // 12 recorded sessions, newest first; the current one is not in
        // the newest five, so the picker opens on an older row that the
        // window would otherwise hide.
        let sessions: Vec<_> = (0..12)
            .map(|n| session_file(&format!("00000000{n:02}-m{n}-0"), &format!("m{n}")))
            .collect();
        app.cfg.session_id = Some("0000000005-m5-0".into());
        app.overlay = Some(Overlay::Sessions {
            sessions,
            picker: ListPicker::with_selected(12, 5).actions(SESSION_ACTIONS),
        });

        // The window shows a bounded slice of the list with the
        // position in the header — the /models display grammar — and
        // the older sessions are still reachable by paging.
        let lines = live_lines(&app, 260)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone().into_owned()))
            .collect::<String>();
        assert!(lines.contains("6/12"), "position shown: {lines}");
        assert!(lines.contains("enter resume"), "key hints: {lines}");
        assert!(lines.contains("(current)"), "marks current: {lines}");

        // Page down past the window into the older rows: the cursor
        // moves without closing the picker.
        let pgdn = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        handle_overlay_key(&mut app, pgdn, &worker);
        match &app.overlay {
            Some(Overlay::Sessions { picker, .. }) => assert_eq!(picker.index(), 11),
            _ => panic!("sessions overlay stays open"),
        }
    }

    #[tokio::test]
    async fn session_loaded_replays_the_transcript() {
        let mut app = ext_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        let call = orca_harness_core::ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"command": "ls"}),
        };
        let messages = vec![
            orca_harness_core::Message::System {
                content: "sys".into(),
            },
            orca_harness_core::Message::User {
                content: "first prompt".into(),
            },
            orca_harness_core::Message::Assistant {
                content: None,
                tool_calls: vec![call.clone()],
            },
            orca_harness_core::Message::Tool {
                results: vec![orca_harness_core::ToolResult::ok(
                    &call,
                    serde_json::json!({"stdout": "a\n", "success": true}),
                )],
            },
            orca_harness_core::Message::Assistant {
                content: Some("the answer".into()),
                tool_calls: vec![],
            },
        ];
        handle_ui_msg(
            &mut app,
            UiMsg::SessionLoaded {
                id: "s1".into(),
                messages,
            },
            &worker,
            80,
        );

        assert_eq!(app.cfg.session_id.as_deref(), Some("s1"));
        let text = printed(&app);
        assert!(text.contains("resumed session s1 (5 messages)"), "{text}");
        assert!(text.contains("┃ first prompt"), "spine replayed: {text}");
        assert!(text.contains("shell"), "tool call replayed: {text}");
        assert!(text.contains("the answer"), "answer replayed: {text}");
    }
}

#[cfg(test)]
mod mcp_command_tests {
    use super::*;

    fn mcp_app() -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
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
        })
    }

    fn printed(app: &App) -> String {
        app.pending_history
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(app: &mut App, tx: &mpsc::UnboundedSender<WorkerCmd>, code: KeyCode) {
        handle_terminal_event(
            app,
            CtEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            tx,
            80,
        );
    }

    /// The overlay as drawn, one string per line.
    fn overlay_text(app: &App) -> String {
        live_lines(app, 120)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn add_list_and_remove_round_trip_through_config_and_reload() {
        let mut app = mcp_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // Bare form with nothing configured points at the add syntax
        // rather than opening an overlay with no rows to toggle.
        slash_command(&mut app, "mcp", &worker, 80);
        assert!(printed(&app).contains("no MCP servers configured"));
        assert!(app.overlay.is_none());

        // Add saves the command verbatim (arguments included) and asks
        // the worker to reconnect.
        slash_command(
            &mut app,
            "mcp add docs npx -y some-server /tmp",
            &worker,
            80,
        );
        let stored = crate::config::stored_mcp_servers();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "docs");
        assert_eq!(stored[0].command, "npx -y some-server /tmp");
        assert!(stored[0].enabled, "a new server starts on");
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadMcp)));
        assert!(printed(&app).contains("mcp server docs added"));

        // Bare form now opens the picker, listing state and command.
        // The count is "…" until a reload reports one.
        slash_command(&mut app, "mcp", &worker, 80);
        assert!(matches!(app.overlay, Some(Overlay::Mcp { .. })));
        let text = overlay_text(&app);
        assert!(text.contains("space toggle"), "{text}");
        assert!(text.contains("docs  on   …"), "{text}");
        assert!(text.contains("npx -y some-server /tmp"), "{text}");
        press(&mut app, &worker, KeyCode::Esc);

        // Remove drops it and reconnects; "rm" and "delete" are aliases.
        slash_command(&mut app, "mcp remove docs", &worker, 80);
        assert!(crate::config::stored_mcp_servers().is_empty());
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadMcp)));
        assert!(printed(&app).contains("mcp server docs removed"));
    }

    /// Space toggles the selected row: the config is written, the
    /// worker is asked to reconnect, and the overlay stays open showing
    /// the new state at once.
    #[tokio::test]
    async fn space_toggles_the_selected_server_and_the_overlay_stays_open() {
        let mut app = mcp_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server("docs", "run docs").unwrap();
        crate::config::save_mcp_server("fetch", "run fetch").unwrap();

        slash_command(&mut app, "mcp", &worker, 80);
        // Config order is the map's: docs, then fetch.
        press(&mut app, &worker, KeyCode::Down);
        press(&mut app, &worker, KeyCode::Char(' '));

        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadMcp)));
        assert!(
            matches!(app.overlay, Some(Overlay::Mcp { .. })),
            "toggling keeps the overlay open for the next row"
        );
        let stored = crate::config::stored_mcp_servers();
        assert!(stored[0].enabled, "the unselected row is untouched");
        assert!(!stored[1].enabled, "fetch is now off");

        // The row redraws immediately, without waiting for the reload,
        // and an off server shows no tool count.
        let text = overlay_text(&app);
        assert!(text.contains("docs   on   …"), "{text}");
        assert!(text.contains("fetch  off"), "{text}");
        assert!(!text.contains("fetch  off  …"), "off rows drop the count");

        // Enter toggles too, matching /extensions muscle memory.
        press(&mut app, &worker, KeyCode::Enter);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadMcp)));
        assert!(crate::config::stored_mcp_servers()[1].enabled);
        assert!(overlay_text(&app).contains("fetch  on"));

        press(&mut app, &worker, KeyCode::Esc);
        assert!(app.overlay.is_none());
    }

    #[test]
    fn mcp_picker_filters_by_typing_and_reports_position() {
        let mut app = mcp_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server("alpha", "run alpha").unwrap();
        crate::config::save_mcp_server("beta", "run beta").unwrap();

        slash_command(&mut app, "mcp", &worker, 80);
        assert!(overlay_text(&app).contains("1/2"));
        press(&mut app, &worker, KeyCode::Char('b'));
        let text = overlay_text(&app);
        assert!(text.contains("filter: b"), "{text}");
        assert!(text.contains("beta"), "{text}");
        assert!(!text.contains("run alpha"), "{text}");
        assert!(text.contains("1/1"), "{text}");

        press(&mut app, &worker, KeyCode::Backspace);
        assert!(overlay_text(&app).contains("1/2"));
        crate::config::remove_mcp_server("alpha").unwrap();
        crate::config::remove_mcp_server("beta").unwrap();
    }

    /// Tool counts and connection errors come off the shared handle the
    /// worker reloads. The reason goes after the command so a long error
    /// never truncates the row before the command is visible.
    #[tokio::test]
    async fn rows_report_tool_counts_and_connection_errors() {
        let mut app = mcp_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server("ghost", "orca-no-such-binary-xyz").unwrap();
        app.cfg.mcp.reload().await;

        slash_command(&mut app, "mcp", &worker, 80);
        let text = overlay_text(&app);
        assert!(text.contains("ghost  on   failed"), "{text}");
        assert!(
            text.contains("failed     orca-no-such-binary-xyz"),
            "the command survives the error: {text}"
        );
        assert!(text.contains("spawn failed"), "the reason is shown: {text}");
    }

    /// Env references survive redaction — they name a variable, they do
    /// not carry it — so the row still says which one a server needs.
    #[test]
    fn redaction_keeps_env_references_and_the_rest_of_the_command() {
        let command = "npx -y mcp-remote https://api.githubcopilot.com/mcp/readonly \
                       --header Authorization:${AUTH_HEADER}";
        assert_eq!(redact_command(command), command);
    }

    #[test]
    fn redaction_masks_literal_credentials_in_every_shape() {
        // The shape a user actually produces: `--header` values cannot
        // contain spaces (the command is whitespace-split), so a pasted
        // credential arrives glued to the header name. The name survives
        // so the row still says what is being sent.
        assert_eq!(
            redact_command("npx mcp-remote https://x.dev/mcp --header Authorization:ghp_realtoken"),
            "npx mcp-remote https://x.dev/mcp --header Authorization:<redacted>"
        );
        // Being an Authorization value is enough on its own — the value
        // need not look token-shaped.
        assert_eq!(
            redact_command("x --header Authorization:Bearer"),
            "x --header Authorization:<redacted>"
        );
        // An unrecognized header name masks the whole word rather than
        // guessing which half is the secret; losing the name is the safe
        // direction.
        assert_eq!(
            redact_command("x --header X-Custom-Auth:ghp_realtoken"),
            "x --header <redacted>"
        );
        // Flag and value in one word.
        assert_eq!(
            redact_command("some-server --api-key=sk-abc123"),
            "some-server --api-key=<redacted>"
        );
        // Flag and value split across words.
        assert_eq!(
            redact_command("some-server --token sk-abc123"),
            "some-server --token <redacted>"
        );
        // A bare token as a positional argument.
        assert_eq!(
            redact_command("some-server github_pat_11ABCDE"),
            "some-server <redacted>"
        );
        // Credentials inside the URL: query parameter and userinfo.
        assert_eq!(
            redact_command("npx mcp-remote https://x.dev/sse?api_key=abc123&mode=fast"),
            "npx mcp-remote https://x.dev/sse?api_key=<redacted>&mode=fast"
        );
        assert_eq!(
            redact_command("npx mcp-remote https://user:hunter2@x.dev/mcp"),
            "npx mcp-remote https://user:<redacted>@x.dev/mcp"
        );
    }

    /// Redaction must not chew up ordinary commands: no flags, no
    /// tokens, nothing that merely looks like one.
    #[test]
    fn redaction_leaves_ordinary_commands_alone() {
        for command in [
            "uvx mcp-server-fetch",
            "npx -y @modelcontextprotocol/server-everything",
            "npx -y mcp-remote https://mcp.context7.com/mcp",
            "npx -y mcp-remote https://gitmcp.io/okikorg/orca",
            "github-mcp-server stdio --toolsets repos,issues",
        ] {
            assert_eq!(redact_command(command), command, "mangled: {command}");
        }
    }

    /// The overlay renders the redacted form, never the stored one.
    #[tokio::test]
    async fn the_picker_never_renders_a_literal_token() {
        let mut app = mcp_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server(
            "github",
            "npx -y mcp-remote https://x.dev/mcp --header Authorization:ghp_supersecret",
        )
        .unwrap();

        slash_command(&mut app, "mcp", &worker, 80);
        let text = overlay_text(&app);
        assert!(!text.contains("ghp_supersecret"), "token on screen: {text}");
        assert!(text.contains("Authorization:<redacted>"), "{text}");
        // The config keeps the real value — this is display-only.
        assert!(crate::config::stored_mcp_servers()[0]
            .command
            .contains("ghp_supersecret"));
    }

    /// The sequence the overlay exists for: toggle on, the row shows `…`
    /// while the reconnect runs, and the count lands once it reports.
    #[tokio::test]
    async fn a_toggled_on_server_moves_from_the_placeholder_to_its_state() {
        let mut app = mcp_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server("ghost", "orca-no-such-binary-xyz").unwrap();
        crate::config::set_mcp_enabled("ghost", false).unwrap();
        app.cfg.mcp.reload().await;

        slash_command(&mut app, "mcp", &worker, 80);
        assert!(overlay_text(&app).contains("ghost  off"));

        // Toggling on redraws as on with no state yet; the worker has
        // not reconnected.
        press(&mut app, &worker, KeyCode::Char(' '));
        assert!(overlay_text(&app).contains("ghost  on   …"));

        // The worker's reload resolves it, with the overlay still open.
        app.cfg.mcp.reload().await;
        let text = overlay_text(&app);
        assert!(!text.contains('…'), "the placeholder resolves: {text}");
        assert!(text.contains("ghost  on   failed"), "{text}");
    }

    #[tokio::test]
    async fn bad_input_reports_and_sends_nothing() {
        let mut app = mcp_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // add needs both a name and a command.
        slash_command(&mut app, "mcp add", &worker, 80);
        slash_command(&mut app, "mcp add docs", &worker, 80);
        assert!(printed(&app).contains("usage: /mcp"));

        // Names feed the model-facing tool prefix, so junk is rejected.
        slash_command(&mut app, "mcp add bad/name run it", &worker, 80);
        assert!(printed(&app).contains("invalid server name: bad/name"));

        // Removing something that was never added names the valid set.
        slash_command(&mut app, "mcp remove nope", &worker, 80);
        assert!(printed(&app).contains("unknown mcp server: nope"));

        // Re-adding a server the user turned off edits it without
        // enabling it, and says so rather than claiming to connect.
        crate::config::save_mcp_server("docs", "run docs").unwrap();
        crate::config::set_mcp_enabled("docs", false).unwrap();
        slash_command(&mut app, "mcp add docs run other", &worker, 80);
        let stored = crate::config::stored_mcp_servers();
        assert_eq!(stored[0].command, "run other");
        assert!(!stored[0].enabled, "an edit is not an enable");
        assert!(printed(&app).contains("mcp server docs updated — still off"));
        crate::config::remove_mcp_server("docs").unwrap();
        while rx.try_recv().is_ok() {}

        slash_command(&mut app, "mcp frobnicate", &worker, 80);
        assert!(printed(&app).contains("usage: /mcp"));

        assert!(crate::config::stored_mcp_servers().is_empty());
        assert!(rx.try_recv().is_err(), "bad input sends nothing");
    }
}

#[cfg(test)]
mod stats_segment_tests {
    use super::*;

    #[test]
    fn segments_render_only_nonzero_counts() {
        let stats = orca_harness_tools::BackgroundStats::new();
        assert_eq!(stats_segments(&stats), "");
        stats.inc_processes();
        stats.inc_processes();
        stats.inc_agents();
        assert_eq!(stats_segments(&stats), " · procs 2 · agents 1");
        stats.inc_kernels();
        assert_eq!(stats_segments(&stats), " · procs 2 · pykernel · agents 1");
    }
}

#[cfg(test)]
mod nested_rail_tests {
    use super::*;
    #[cfg(test)]
use orca_harness_extensions::HarnessEvent;
    use serde_json::json;

    fn nested_app() -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
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
        })
    }

    fn rail_text(app: &App) -> String {
        activity_lines(app, 120, true)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn inner_tools_render_indented_under_the_subagent_line() {
        let mut app = nested_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "explore"}),
            },
            120,
        );
        handle_subagent_event(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                input: json!({"path": "."}),
            },
        );
        let text = rail_text(&app);
        assert!(text.contains("subagent"), "rail: {text}");
        assert!(text.contains("list_dir"), "rail: {text}");
        let inner_line = text.lines().find(|l| l.contains("list_dir")).unwrap();
        assert!(
            inner_line.starts_with("      "),
            "inner line must be indented: {inner_line:?}"
        );
        assert!(
            inner_line.contains("└─") || inner_line.contains("├─"),
            "inner line must carry a tree branch so ownership is unambiguous: {inner_line:?}"
        );
        let outer_line = text.lines().find(|l| l.contains("subagent")).unwrap();
        let branch_col = |l: &str| l.find(['└', '├']).unwrap();
        assert!(
            branch_col(inner_line) > branch_col(outer_line),
            "inner branch must sit deeper than the subagent's own branch:\n{outer_line}\n{inner_line}"
        );

        handle_subagent_event(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolResult {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                output: json!({"entries": []}),
                is_error: false,
            },
        );
        let text = rail_text(&app);
        let inner_line = text.lines().find(|l| l.contains("list_dir")).unwrap();
        assert!(inner_line.contains("✓"), "completed glyph: {inner_line:?}");
    }

    #[test]
    fn deeper_spawns_indent_further() {
        let mut app = nested_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "outer"}),
            },
            120,
        );
        handle_subagent_event(
            &mut app,
            1,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "i1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "inner"}),
            },
        );
        handle_subagent_event(
            &mut app,
            2,
            Some(1),
            1,
            "i1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "g1".into(),
                tool_name: "grep".into(),
                input: json!({"pattern": "x"}),
            },
        );
        let text = rail_text(&app);
        let child = text
            .lines()
            .find(|l| l.contains("subagent {\"task\":\"inner"))
            .unwrap();
        let grandchild = text.lines().find(|l| l.contains("grep")).unwrap();
        let indent = |l: &str| l.chars().take_while(|c| *c == ' ').count();
        assert!(
            indent(grandchild) > indent(child),
            "child: {child:?} grandchild: {grandchild:?}"
        );
    }

    #[test]
    fn completion_folds_inner_log_into_the_expandable_record() {
        let mut app = nested_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "explore"}),
            },
            120,
        );
        handle_subagent_event(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                input: json!({"path": "."}),
            },
        );
        handle_subagent_event(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolResult {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                output: json!({"entries": []}),
                is_error: false,
            },
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                output: json!({"answer": "found things"}),
                is_error: false,
            },
            120,
        );

        assert!(
            app.subagent_activity.is_empty(),
            "spawn state must fold away"
        );
        let record = app.tool_log.last().unwrap();
        assert!(record.inner.iter().any(|l| l.contains("list_dir")));

        expand_tool(&mut app, 1, 120);
        let expanded: String = app
            .pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded.contains("inner activity"), "{expanded}");
        assert!(expanded.contains("list_dir"), "{expanded}");
    }
}

#[cfg(test)]
mod skills_command_tests {
    use super::*;

    /// A temp tree plus the `Skills` handle that scans it. Roots are
    /// passed in explicitly, so a test never reaches the developer's own
    /// ~/.claude/skills.
    struct Fixture {
        dir: std::path::PathBuf,
        skills: crate::skills::Skills,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "orca-tui-skills-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let skills = crate::skills::Skills::new(&dir, None, None);
            Self { dir, skills }
        }

        fn skill(&self, name: &str, description: &str) {
            let path = self.dir.join(".orca/skills").join(name).join("SKILL.md");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                format!("---\nname: {name}\ndescription: {description}\n---\n\nstep one\n"),
            )
            .unwrap();
            self.skills.reload();
        }

        fn app(&self) -> App {
            App::new(TuiConfig {
                model_name: "m".into(),
                workspace_name: "w".into(),
                workspace_root: "/test-ws".into(),
                provider: Provider::Local,
                subagent_depth: orca_harness_tools::SubagentDepth::new(1),
                stats: orca_harness_tools::BackgroundStats::new(),
                session_id: None,
                mcp: Default::default(),
                skills: self.skills.clone(),
                mode: Default::default(),
                todos: Default::default(),
                plan: Default::default(),
            })
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn printed(app: &App) -> String {
        app.pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn overlay_text(app: &App) -> String {
        live_lines(app, 120)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Nothing found: the bare form says how to get one instead of
    /// opening an overlay with no rows in it.
    #[test]
    fn empty_catalog_points_at_add_and_create() {
        let fixture = Fixture::new("empty");
        let mut app = fixture.app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills", &worker, 80);
        assert!(app.overlay.is_none());
        let text = printed(&app);
        assert!(text.contains("/skills add"), "{text}");
        assert!(text.contains("/skills create"), "{text}");
        assert!(text.contains(".claude/skills"), "{text}");
    }

    fn press(app: &mut App, worker: &mpsc::UnboundedSender<WorkerCmd>, code: KeyCode) {
        handle_terminal_event(
            app,
            CtEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            worker,
            80,
        );
    }

    /// Space reveals the strip rather than acting, so neither toggling
    /// nor deleting is one stray keystroke away.
    #[test]
    fn space_reveals_the_actions_and_t_toggles() {
        let fixture = Fixture::new("toggle");
        fixture.skill("release", "Cut a release");
        let mut app = fixture.app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills", &worker, 80);
        let shown = overlay_text(&app);
        assert!(shown.contains("release"), "{shown}");
        assert!(shown.contains("Cut a release"), "{shown}");
        assert!(shown.contains("space actions"), "{shown}");

        press(&mut app, &worker, KeyCode::Char(' '));
        let armed = overlay_text(&app);
        assert!(armed.contains("[t] toggle"), "{armed}");
        assert!(armed.contains("[d] delete"), "{armed}");
        assert_eq!(
            crate::config::stored_skill_enabled("release"),
            None,
            "space itself changes nothing"
        );

        press(&mut app, &worker, KeyCode::Char('t'));
        assert_eq!(
            crate::config::stored_skill_enabled("release"),
            Some(false),
            "the action key writes the override"
        );
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadSkills)));
        // The overlay stays open and the row redraws off at once, while
        // the rescan and rebuild run behind it.
        assert!(app.overlay.is_some());
        assert!(overlay_text(&app).contains("off"), "{}", overlay_text(&app));

        // Enter keeps the one-key path for the common case.
        press(&mut app, &worker, KeyCode::Enter);
        assert_eq!(crate::config::stored_skill_enabled("release"), Some(true));
    }

    #[test]
    fn skills_picker_filters_by_typing_and_backspace() {
        let fixture = Fixture::new("filter");
        fixture.skill("deploy", "Ship the application");
        fixture.skill("review", "Review a change");
        let mut app = fixture.app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills", &worker, 80);
        assert!(overlay_text(&app).contains("1/2"));
        press(&mut app, &worker, KeyCode::Char('r'));
        let text = overlay_text(&app);
        assert!(text.contains("filter: r"), "{text}");
        assert!(text.contains("review"), "{text}");
        assert!(!text.contains("deploy"), "{text}");
        assert!(text.contains("1/1"), "{text}");

        press(&mut app, &worker, KeyCode::Backspace);
        assert!(overlay_text(&app).contains("1/2"));
    }

    /// Delete removes the folder, the row, and the saved override — and
    /// only for skills this host installed.
    #[test]
    fn delete_action_removes_an_installed_skill() {
        let fixture = Fixture::new("delete");
        fixture.skill("release", "Cut a release");
        let mut app = fixture.app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = fixture.dir.join(".orca/skills/release");
        assert!(dir.is_dir());

        slash_command(&mut app, "skills", &worker, 80);
        press(&mut app, &worker, KeyCode::Char(' '));
        press(&mut app, &worker, KeyCode::Char('d'));

        assert!(!dir.exists(), "the folder is gone");
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadSkills)));
        // Last row deleted: the overlay closes rather than showing an
        // empty list.
        assert!(app.overlay.is_none());
        assert!(
            printed(&app).contains("removed release"),
            "{}",
            printed(&app)
        );
    }

    /// The whole loop against the real internet: install a published
    /// skill from GitHub through `/skills add`, confirm the running
    /// agent is offered it, then delete it from the overlay. Ignored by
    /// default — it clones a repository and spawns the built binary
    /// against a local model endpoint.
    ///
    /// Run with:
    /// `cargo test -p orcacode --bin orcacode e2e_ -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "network: clones a github repository and calls a model"]
    async fn e2e_add_use_and_remove_a_published_skill() {
        let fixture = Fixture::new("e2e");
        let config = fixture.dir.join("config");
        let workspace = fixture.dir.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("config.json"),
            r#"{"provider": "local", "models": {"local": "gemma4:e2b-mlx"}}"#,
        )
        .unwrap();
        let skills = crate::skills::Skills::new(&workspace, Some(config.clone()), None);
        let mut app = App::new(TuiConfig {
            model_name: "e2e".into(),
            workspace_name: "ws".into(),
            workspace_root: workspace.display().to_string(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: skills.clone(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        });
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // 1. Add, exactly as the composer would.
        slash_command(
            &mut app,
            "skills add vercel-labs/agent-skills --skill writing-guidelines",
            &worker,
            80,
        );
        let Ok(WorkerCmd::InstallSkill { source, here }) = rx.try_recv() else {
            panic!("no install command: {}", printed(&app));
        };
        assert!(!here, "installs beside config.json by default");
        // What the worker does with it.
        let lines = skills.add(&source, here).await.expect("install");
        println!("{}", lines.join("\n"));
        skills.reload();
        let installed = config.join("skills/writing-guidelines/SKILL.md");
        assert!(installed.is_file(), "SKILL.md landed at {installed:?}");

        // 2. The running agent is offered it, by name, with its blurb.
        let tool = skills.tool().expect("a skill tool");
        let schema = tool.schema();
        assert_eq!(schema.name, "skill");
        assert!(
            schema.description.contains("writing-guidelines"),
            "{}",
            schema.description
        );

        // 3. A real model, given the real binary, calls it. The tool log
        //    goes to stderr, so that is where the call shows up.
        // The test binary lives in target/<profile>/deps/, so the CLI
        // it was built alongside is two directories up.
        let test_binary = std::env::current_exe().expect("test binary path");
        let binary = test_binary
            .parent()
            .and_then(std::path::Path::parent)
            .expect("target dir")
            .join("orcacode");
        assert!(binary.is_file(), "build the binary first: {binary:?}");
        let run = std::process::Command::new(&binary)
            .env("ORCA_CONFIG_DIR", &config)
            .args(["--workspace"])
            .arg(&workspace)
            .args([
                "--no-session",
                "--auto-approve",
                "--max-steps",
                "4",
                "-p",
                "Load the writing-guidelines skill and quote its first heading. \
                 Use the skill tool.",
            ])
            .output()
            .expect("run orcacode");
        let stderr = String::from_utf8_lossy(&run.stderr);
        let stdout = String::from_utf8_lossy(&run.stdout);
        println!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
        assert!(
            stderr.contains("skill"),
            "the model never reached for the skill tool"
        );

        // 4. Remove it from the overlay: space reveals, d deletes.
        slash_command(&mut app, "skills", &worker, 80);
        let shown = overlay_text(&app);
        assert!(shown.contains("writing-guidelines"), "{shown}");
        press(&mut app, &worker, KeyCode::Char(' '));
        press(&mut app, &worker, KeyCode::Char('d'));
        assert!(
            !config.join("skills/writing-guidelines").exists(),
            "the folder is gone"
        );
        skills.reload();
        assert!(skills.tool().is_none(), "and so is the tool");
    }

    /// A skill from a compatibility root is not this host's to delete.
    #[test]
    fn delete_refuses_a_skill_from_a_root_it_does_not_own() {
        let fixture = Fixture::new("foreign");
        let path = fixture.dir.join(".claude/skills/borrowed/SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nname: borrowed\ndescription: someone else's\n---\n\nbody\n",
        )
        .unwrap();
        fixture.skills.reload();
        let mut app = fixture.app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills remove borrowed", &worker, 80);
        assert!(path.is_file(), "the file must survive");
        let text = printed(&app);
        assert!(text.contains("only deletes what it installed"), "{text}");
    }

    #[test]
    fn show_reports_one_skill_and_rejects_unknown_names() {
        let fixture = Fixture::new("show");
        fixture.skill("release", "Cut a release");
        let mut app = fixture.app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills show release", &worker, 80);
        let text = printed(&app);
        assert!(text.contains(".orca/skills"), "{text}");
        assert!(text.contains("Cut a release"), "{text}");

        slash_command(&mut app, "skills show nope", &worker, 80);
        let text = printed(&app);
        assert!(text.contains("unknown skill: nope"), "{text}");
        assert!(text.contains("found: release"), "{text}");
    }

    #[test]
    fn reload_goes_through_the_worker_and_garbage_is_rejected() {
        let fixture = Fixture::new("reload");
        let mut app = fixture.app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills reload", &worker, 80);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadSkills)));
        assert!(printed(&app).contains("rescanning skills"));

        slash_command(&mut app, "skills wat", &worker, 80);
        assert!(rx.try_recv().is_err(), "no command for a bad argument");
        assert!(printed(&app).contains("unknown /skills argument: wat"));
    }
}
