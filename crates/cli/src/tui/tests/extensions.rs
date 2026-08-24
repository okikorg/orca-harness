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

