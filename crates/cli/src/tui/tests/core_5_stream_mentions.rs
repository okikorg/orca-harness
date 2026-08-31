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
                reasoning: None,
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
    fn location_picker_searches_entries_beyond_the_visible_window() {
        let mut app = test_app();
        let entries = (0..PICKER_ROWS + 1)
            .map(|index| LocationEntry {
                path: format!("folder-{index}/file.txt"),
                directory: false,
            })
            .collect();
        app.overlay = Some(Overlay::Locations(LocationPicker {
            picker: ListPicker::new(PICKER_ROWS + 1),
            entries,
            query: "folder-10".into(),
            token_start: 0,
        }));

        let Some(Overlay::Locations(location)) = &app.overlay else {
            panic!("expected @ location picker");
        };
        assert_eq!(location.filtered().len(), 1);
        assert_eq!(location.selected().unwrap().path, "folder-10/file.txt");
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
            app.turn_tokens_in, 9,
            "input progress estimates the normalized submitted prompt once"
        );
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
