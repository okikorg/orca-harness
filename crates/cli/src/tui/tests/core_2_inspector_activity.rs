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
        execution_started: None,
        execution_elapsed: None,
        elapsed: Some(Duration::from_millis(1)),
        output: Some(serde_json::json!({"path": "README.md", "bytesWritten": 26})),
        is_error: false,
        approval: None,
    };
    let inspector = flat_lines(&tool_inspector_lines(&tool, 80));
    assert!(inspector.contains("content · markdown · 3 lines · 26 B"));
    assert!(inspector.contains("README.md"));
    assert!(inspector.contains("# Orca"));
    assert!(inspector.contains("    indented code"));
    assert!(!inspector.contains("\\n"));
    assert!(inspector.contains("written"));
    assert!(inspector.contains("26 B"));
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
        execution_started: None,
        execution_elapsed: None,
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

#[test]
fn inspector_marks_live_finished_tool_complete_before_output_arrives() {
    let started = Instant::now();
    let tool = ToolActivity {
        call_id: "glob-1".into(),
        call_line: "glob *.rs".into(),
        tool_name: "glob".into(),
        input: serde_json::json!({"pattern": "*.rs"}),
        started,
        execution_started: Some(started),
        execution_elapsed: Some(Duration::from_millis(2)),
        elapsed: Some(Duration::from_millis(2)),
        output: None,
        is_error: false,
        approval: None,
    };

    let inspector = flat_lines(&tool_inspector_lines(&tool, 80));
    assert!(
        inspector.contains("glob · completed · preflight 0ns · run 2ms"),
        "{inspector}"
    );
    assert!(!inspector.contains("running"), "{inspector}");
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
    assert_eq!(notice.spans[0].style, theme().dim);
    assert_eq!(notice.spans[1].style, theme().dim);
}

/// A refused command is still the system talking. Without the shared
/// glyph it renders flush-left against the notices around it and
/// reads as model output. The glyph stays the same; the error label and
/// color distinguish severity even in monochrome.
#[test]
fn errors_use_the_same_glyph_as_notices_with_an_error_body() {
    let _theme = THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
    let mut app = test_app();

    push_notice(&mut app, "plan mode · read-only");
    push_error(&mut app, "unknown mode: pkan");

    let notice = &app.pending_history[app.pending_history.len() - 2];
    let error = app.pending_history.last().expect("error line");
    assert_eq!(line_text(error), "• error: unknown mode: pkan");
    // Same leading glyph, so both lines start in the same column.
    assert_eq!(line_text(notice).chars().next(), Some('•'));
    assert_eq!(error.spans[0].style, theme().error);
    // Severity is the body's job, and it differs from a notice.
    assert_eq!(error.spans[1].style, theme().error);
    assert_ne!(error.spans[1].style, notice.spans[1].style);
}

#[test]
fn turn_token_estimator_is_chunk_invariant() {
    let text = "split 你好 inside words";
    let mut one_shot = super::format::TokenEstimator::default();
    one_shot.consume(text);

    for split in text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
    {
        let mut fragmented = super::format::TokenEstimator::default();
        fragmented.consume(&text[..split]);
        fragmented.consume(&text[split..]);
        assert_eq!(fragmented.estimate(), one_shot.estimate());
    }
}

#[test]
fn elapsed_labels_keep_sub_millisecond_tool_timings_visible() {
    assert_eq!(elapsed_label(Duration::ZERO), "0ns");
    assert_eq!(elapsed_label(Duration::from_nanos(850)), "850ns");
    assert_eq!(elapsed_label(Duration::from_micros(842)), "842µs");
    assert_eq!(elapsed_label(Duration::from_millis(14)), "14ms");
    assert_eq!(elapsed_label(Duration::from_millis(1_500)), "1.5s");
    assert_eq!(elapsed_label(Duration::from_secs(60)), "1m 0s");
    assert_eq!(elapsed_label(Duration::from_secs(199)), "3m 19s");
}

#[test]
fn tool_timing_separates_review_from_execution() {
    let started = Instant::now();
    let tool = ToolActivity {
        call_id: "edit-1".into(),
        call_line: "multi_edit 2 edits · 2 files".into(),
        tool_name: "multi_edit".into(),
        input: serde_json::json!({}),
        started,
        execution_started: Some(started + Duration::from_millis(2_300)),
        execution_elapsed: Some(Duration::from_micros(219)),
        elapsed: Some(Duration::from_micros(2_300_219)),
        output: Some(serde_json::json!({"filesChanged": 2})),
        is_error: false,
        approval: None,
    };

    // The inspector attributes the wait; a row shows the wait the reader
    // felt when the run itself was instant.
    assert_eq!(
        tool_timing_label(&tool, true),
        "preflight 2.3s · run 219µs"
    );
    assert_eq!(tool_timing_label(&tool, false), "2.3s");
}

#[test]
fn live_tool_completion_freezes_elapsed_before_ordered_result_arrives() {
    let mut app = test_app();
    handle_harness_event(
        &mut app,
        HarnessEvent::ToolCall {
            tool_call_id: "fast".into(),
            tool_name: "glob".into(),
            input: serde_json::json!({"pattern": "*.rs"}),
        },
        80,
    );
    handle_harness_event(
        &mut app,
        HarnessEvent::ToolStarted {
            tool_call_id: "fast".into(),
            tool_name: "glob".into(),
        },
        80,
    );
    handle_harness_event(
        &mut app,
        HarnessEvent::ToolFinished {
            tool_call_id: "fast".into(),
            tool_name: "glob".into(),
            is_error: false,
        },
        80,
    );
    let frozen = app.activity_tools[0]
        .elapsed
        .expect("completion freezes time");
    std::thread::sleep(Duration::from_millis(5));
    handle_harness_event(
        &mut app,
        HarnessEvent::ToolResult {
            tool_call_id: "fast".into(),
            tool_name: "glob".into(),
            output: serde_json::json!({"matches": []}),
            is_error: false,
        },
        80,
    );

    assert_eq!(app.activity_tools[0].elapsed, Some(frozen));
    assert!(app.activity_tools[0].output.is_some());
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
    assert!(joined.contains("Shell · $ ls"));
    assert!(joined.contains("✓ Shell · $ ls · exit 0 · a.rs"));
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
            id: crate::msg::RunId::User(1),
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
    assert!(joined.contains("Read · a.rs"));
    let record = app.tool_log.last().expect("thinking recorded");
    assert_eq!(record.tool_name, "thinking");
    assert_eq!(record.output, serde_json::json!("let me check the file"));
    assert!(app.reasoning.is_empty(), "buffer reset after flush");
}

#[test]
fn interrupted_thinking_still_lands_in_the_rail() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    app.run = RunState::Running {
        id: crate::msg::RunId::User(1),
        started: Instant::now(),
        cancel: CancellationToken::new(),
    };
    handle_harness_event(
        &mut app,
        HarnessEvent::ReasoningDelta { text: "hmm".into() },
        80,
    );
    handle_ui_msg(&mut app, UiMsg::RunDone { id: crate::msg::RunId::User(1), result: Err("cancelled".into()) }, &tx, 80);
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
