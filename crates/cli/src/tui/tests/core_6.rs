#[test]
fn split_view_connects_the_selected_tool_to_its_inspector() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    app.view_mode = ViewMode::Split;
    let empty = rendered_rows(&mut app, 120, 24).join("\n");
    assert!(
        empty.contains("Tool Inspector"),
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
        inspector.contains("shell") && inspector.contains("running"),
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
    assert!(inspector.contains("Waiting for result"));

    // Ctrl+Tab has no application behavior. Plain Tab remains slash-command
    // completion even while the split inspector is visible.
    app.composer = "/he".into();
    app.cursor = app.composer.chars().count();
    handle_terminal_event(
        &mut app,
        CtEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL)),
        &tx,
        120,
    );
    assert_eq!(app.composer, "/he");

    // Modal handlers use plain Tab too, but Ctrl+Tab is consumed before they
    // can treat it as completion/activation.
    app.overlay = Some(Overlay::Locations(LocationPicker {
        entries: vec![LocationEntry {
            path: "crates/cli".into(),
            directory: true,
        }],
        query: String::new(),
        token_start: 0,
        picker: ListPicker::new(1),
    }));
    handle_terminal_event(
        &mut app,
        CtEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL)),
        &tx,
        120,
    );
    assert!(matches!(app.overlay, Some(Overlay::Locations(_))));
    app.overlay = None;

    handle_terminal_event(
        &mut app,
        CtEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        &tx,
        120,
    );
    assert_eq!(app.composer, "/help");
    app.composer.clear();
    app.cursor = 0;
    let mode = app.inspector_mode;
    press(&mut app, &tx, KeyCode::Char('d'));
    assert_eq!(app.inspector_mode, mode, "d does not switch inspector mode");
    assert_eq!(app.composer, "d", "d remains ordinary composer input");
    app.composer.clear();
    app.cursor = 0;

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
    // A retained snapshot stays mounted without a separate focus mode.
    assert!(app.split_snapshot.is_some());
    let idle_screen = rendered_rows(&mut app, 120, 24).join("\n");
    assert!(!idle_screen.contains("ctrl+tab"), "{idle_screen}");
    assert!(!idle_screen.contains("esc interrupt"), "{idle_screen}");
    app.split_scroll = 10;
    let settled = rendered_rows(&mut app, 120, 24).join("\n");
    assert!(
        settled.contains("shell") && settled.contains("result"),
        "the pane stays mounted and its header stays pinned: {settled}"
    );
}

#[test]
fn long_python_inspector_keeps_a_right_gutter_in_summary_and_debug() {
    let mut app = test_app();
    app.view_mode = ViewMode::Split;
    let code = [
        "import numpy as np",
        "",
        "# Deliberately longer than the inspector pane to exercise wrapping",
        "slope = np.sum((X - X_mean) * (y - y_mean)) / np.sum((X - X_mean) ** 2)",
        "print(f\"Calculated slope: {slope:.4f}; fitted equation and diagnostics follow\")",
    ]
    .join("\n");
    handle_harness_event(
        &mut app,
        HarnessEvent::ToolCall {
            tool_call_id: "python-1".into(),
            tool_name: "pykernel".into(),
            input: serde_json::json!({"action": "exec", "code": code}),
        },
        120,
    );
    handle_harness_event(
        &mut app,
        HarnessEvent::ToolResult {
            tool_call_id: "python-1".into(),
            tool_name: "pykernel".into(),
            output: serde_json::json!({
                "state": "ok",
                "output": "Calculated slope: 2.9080\nFitted equation: y = 2.9080x + 5.4302"
            }),
            is_error: false,
        },
        120,
    );

    fn render(app: &mut App) -> (Vec<String>, Vec<String>) {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        let pane_x = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(ratatui::layout::Rect::new(0, 0, 120, 24))[1]
            .x;
        let full = (0..24)
            .map(|y| {
                (0..120)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let pane = (0..24)
            .map(|y| {
                (pane_x..120)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        (full, pane)
    }

    let (summary_screen, summary_pane) = render(&mut app);
    let summary_text = summary_pane.join("\n");
    assert!(summary_text.contains("code · python"));
    assert!(summary_text.contains("result · ok"));
    assert!(summary_text.contains("summary"));
    assert!(
        summary_pane
            .iter()
            .any(|row| row.starts_with("│  │ import")),
        "source uses one shared body inset: {summary_text}"
    );
    for (y, row) in summary_screen.iter().enumerate() {
        assert_eq!(
            row.chars().nth(119),
            Some(' '),
            "summary consumed its right gutter at row {y}: {row:?}"
        );
    }
    eprintln!(
        "SUMMARY INSPECTOR\n{}",
        summary_pane
            .iter()
            .map(|row| format!("{}▏", row.trim_end()))
            .collect::<Vec<_>>()
            .join("\n")
    );

    app.inspector_mode = InspectorMode::Debug;
    app.split_inspector_cache = None;
    let (debug_screen, debug_pane) = render(&mut app);
    let debug_text = debug_pane.join("\n");
    assert!(debug_text.contains("input"));
    assert!(debug_text.contains("output"));
    assert!(debug_text.contains("debug"));
    assert!(!debug_text.contains("d summary"));
    for (y, row) in debug_screen.iter().enumerate() {
        assert_eq!(
            row.chars().nth(119),
            Some(' '),
            "debug consumed its right gutter at row {y}: {row:?}"
        );
    }
    eprintln!(
        "DEBUG INSPECTOR\n{}",
        debug_pane
            .iter()
            .map(|row| format!("{}▏", row.trim_end()))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let mut file_app = test_app();
    file_app.view_mode = ViewMode::Split;
    file_app.inspector_mode = InspectorMode::Summary;
    handle_harness_event(
        &mut file_app,
        HarnessEvent::ToolCall {
            tool_call_id: "read-1".into(),
            tool_name: "read_file".into(),
            input: serde_json::json!({"path": "benchmarks/kernel.sh"}),
        },
        120,
    );
    handle_harness_event(
        &mut file_app,
        HarnessEvent::ToolResult {
            tool_call_id: "read-1".into(),
            tool_name: "read_file".into(),
            output: serde_json::json!({
                "content": "#!/usr/bin/env bash\n# Kernel overhead benchmarks — the numbers that actually matter here.\nset -euo pipefail",
                "bytes": 112,
                "truncated": false
            }),
            is_error: false,
        },
        120,
    );
    let (file_screen, file_pane) = render(&mut file_app);
    let file_text = file_pane.join("\n");
    assert!(file_text.contains("source · bash"));
    assert!(
        file_pane
            .iter()
            .any(|row| row.starts_with("│  │ #!/usr/bin/env bash")),
        "file source uses one shared body inset: {file_text}"
    );
    for (y, row) in file_screen.iter().enumerate() {
        assert_eq!(
            row.chars().nth(119),
            Some(' '),
            "file preview consumed its right gutter at row {y}: {row:?}"
        );
    }
    eprintln!(
        "FILE INSPECTOR\n{}",
        file_pane
            .iter()
            .map(|row| format!("{}▏", row.trim_end()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn split_divider_runs_through_composer_and_status_rows() {
    let mut app = test_app();
    app.view_mode = ViewMode::Split;
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let inspector_x = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
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
        picker: ListPicker::with_selected(SETTINGS_ROWS, 6),
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
        picker: ListPicker::with_selected(SETTINGS_ROWS, 6),
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

    let rendered = live_lines(&app, 100);
    let listing = flat_lines(&rendered);
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
