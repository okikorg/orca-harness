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

    handle_terminal_event(
        &mut app,
        mouse(MouseEventKind::ScrollUp),
        &tx,
        TEST_TERMINAL_WIDTH,
    );
    assert!(app.scroll > 0, "wheel up scrolls back");
    assert!(app.composer.is_empty(), "composer untouched by wheel");
    assert_eq!(app.history_pos, None, "history untouched by wheel");

    let scrolled = app.scroll;
    handle_terminal_event(
        &mut app,
        mouse(MouseEventKind::ScrollDown),
        &tx,
        TEST_TERMINAL_WIDTH,
    );
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

    handle_terminal_event(&mut app, ctrl('y'), &tx, TEST_TERMINAL_WIDTH);
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
fn command_palette_uses_the_shared_tabular_window() {
    let mut app = test_app();
    app.composer = "/".into();
    app.cursor = 1;
    let lines = palette_lines(&app, PALETTE_ROWS + 2, 100);
    let text: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();

    assert!(text[0].contains(&format!(
        "1/{}",
        crate::tui::command_catalog::COMMANDS.len()
    )));
    let help = text.iter().find(|line| line.contains("/help")).unwrap();
    let clear = text.iter().find(|line| line.contains("/clear")).unwrap();
    let help_detail = help[..help.find("show available").unwrap()].chars().count();
    let clear_detail = clear[..clear.find("preserve this session").unwrap()]
        .chars()
        .count();
    assert_eq!(help_detail, clear_detail);
    assert!(help.contains("▸ /help"), "{help}");
}

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
    assert!(window(&app).contains(&format!("1/{total}")));

    // One step past the last visible row slides the window by one.
    for _ in 0..PALETTE_ROWS {
        handle_terminal_event(
            &mut app,
            CtEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            &tx,
            TEST_TERMINAL_WIDTH,
        );
    }
    assert_eq!(app.palette_picker.index(), PALETTE_ROWS);
    assert!(window(&app).contains(&format!("{}/{total}", PALETTE_ROWS + 1)));
    assert_eq!(app.scroll, 0, "the transcript stays put");

    // Page keys move a screenful of the list, not of the transcript.
    handle_terminal_event(
        &mut app,
        CtEvent::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
        &tx,
        TEST_TERMINAL_WIDTH,
    );
    assert_eq!(app.palette_picker.index(), 0);
    assert_eq!(app.scroll, 0, "page keys do not reach the transcript");
    handle_terminal_event(
        &mut app,
        CtEvent::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
        &tx,
        TEST_TERMINAL_WIDTH,
    );
    assert_eq!(app.palette_picker.index(), PALETTE_ROWS);

    // The wheel does the same, one row at a time, and is clamped.
    handle_terminal_event(
        &mut app,
        mouse(MouseEventKind::ScrollUp),
        &tx,
        TEST_TERMINAL_WIDTH,
    );
    assert_eq!(app.palette_picker.index(), PALETTE_ROWS - 1);
    assert_eq!(app.scroll, 0, "the wheel does not reach the transcript");
    for _ in 0..total * 2 {
        handle_terminal_event(
            &mut app,
            mouse(MouseEventKind::ScrollDown),
            &tx,
            TEST_TERMINAL_WIDTH,
        );
    }
    assert_eq!(
        app.palette_picker.index(),
        total - 1,
        "clamped at the last entry"
    );
    for _ in 0..total * 2 {
        handle_terminal_event(
            &mut app,
            mouse(MouseEventKind::ScrollUp),
            &tx,
            TEST_TERMINAL_WIDTH,
        );
    }
    assert_eq!(
        app.palette_picker.index(),
        0,
        "clamped at the first entry"
    );
    assert_eq!(app.scroll, 0);
}

#[test]
fn mouse_wheel_over_split_inspector_scrolls_only_the_inspector() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    app.view_mode = ViewMode::Split;
    let transcript_scroll = app.scroll;
    // The wheel is routed by where the last frame drew the inspector.
    rendered_rows(&mut app, 120, 24);

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
