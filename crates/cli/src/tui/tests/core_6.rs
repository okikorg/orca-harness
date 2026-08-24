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
