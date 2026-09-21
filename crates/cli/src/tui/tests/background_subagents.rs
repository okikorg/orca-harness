#[test]
fn detached_agent_keeps_a_bounded_semantic_transcript_until_clear() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_test_subagent(&mut app, &worker);

    handle_ui_msg(
        &mut app,
        UiMsg::Event(HarnessEvent::ToolResult {
            tool_call_id: "outer-7".into(),
            tool_name: "subagent".into(),
            output: serde_json::json!({
                "spawnId": 7,
                "status": "running",
                "termination": "detached"
            }),
            is_error: false,
        }),
        &worker,
        100,
    );
    handle_ui_msg(
        &mut app,
        UiMsg::SubagentEvent {
            id: 7,
            parent_id: None,
            depth: 0,
            call_id: "outer-7".into(),
            event: HarnessEvent::AssistantDelta {
                text: "checking".into(),
            },
        },
        &worker,
        100,
    );
    handle_ui_msg(
        &mut app,
        UiMsg::SubagentEvent {
            id: 7,
            parent_id: None,
            depth: 0,
            call_id: "outer-7".into(),
            event: HarnessEvent::Assistant {
                message: "queue is ordered".into(),
            },
        },
        &worker,
        100,
    );
    handle_ui_msg(
        &mut app,
        UiMsg::SubagentEvent {
            id: 7,
            parent_id: None,
            depth: 0,
            call_id: "outer-7".into(),
            event: HarnessEvent::Usage {
                usage: orca_harness_core::Usage {
                    input_tokens: 12,
                    output_tokens: 4,
                    reasoning_tokens: Some(3),
                    ..Default::default()
                },
            },
        },
        &worker,
        100,
    );
    handle_ui_msg(
        &mut app,
        UiMsg::SubagentEvent {
            id: 7,
            parent_id: None,
            depth: 0,
            call_id: "outer-7".into(),
            event: HarnessEvent::Result {
                message: "queue is ordered".into(),
            },
        },
        &worker,
        100,
    );

    let transcript = app.subagent_transcripts.get(&7).unwrap();
    assert!(transcript.detached);
    assert_eq!(
        transcript.status,
        crate::tui::state::SubagentTranscriptStatus::Completed
    );
    assert_eq!(transcript.input_tokens, 12);
    assert_eq!(transcript.output_tokens, 4);
    assert_eq!(transcript.reasoning_tokens, Some(3));
    assert_eq!(
        transcript.entries.len(),
        1,
        "stream and final must not duplicate"
    );
    assert!(!app.subagent_activity.contains_key(&7));

    reset_conversation_ui(&mut app);
    assert!(app.subagent_transcripts.is_empty());
}

#[test]
fn sidekick_commands_map_tiers_and_require_tasks_without_switching_mode() {
    let (worker, mut rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    let mode = app.cfg.mode.get();
    for (command, expected) in [
        ("sidekick inspect", None),
        ("sidekick local inspect", Some("local")),
        ("sidekick fast inspect", Some("flash")),
        ("sidekick mid inspect", Some("mid")),
        ("sidekick frontier inspect", Some("frontier")),
    ] {
        slash_command(&mut app, command, &worker, 80);
        match rx.try_recv() {
            Ok(WorkerCmd::SidekickStart { task, tier }) => {
                assert_eq!(task, "inspect");
                assert_eq!(tier.as_deref(), expected);
            }
            _ => panic!("expected sidekick start"),
        }
    }
    slash_command(&mut app, "sidekick", &worker, 80);
    assert!(last_notice(&app).contains("usage: /sidekick"));
    slash_command(&mut app, "sidekick fast", &worker, 80);
    assert!(last_notice(&app).contains("usage: /sidekick"));
    assert_eq!(app.cfg.mode.get(), mode);
}

#[test]
fn sidekick_stop_picker_lists_live_busy_and_idle_only_and_enter_stops() {
    let (worker, mut rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    for (id, status) in [
        (1, crate::tui::state::SubagentTranscriptStatus::Running),
        (2, crate::tui::state::SubagentTranscriptStatus::Idle),
        (3, crate::tui::state::SubagentTranscriptStatus::Stopped),
        (4, crate::tui::state::SubagentTranscriptStatus::Failed),
    ] {
        let mut transcript = crate::tui::state::SubagentTranscript::new(
            id, None, 0, format!("call-{id}"), format!("task {id}"), None,
        );
        transcript.persistent = true;
        transcript.status = status;
        app.subagent_transcripts.insert(id, transcript);
    }
    slash_command(&mut app, "sidekick stop", &worker, 80);
    let Some(Overlay::SidekickStop { ids, .. }) = &app.overlay else { panic!("picker") };
    assert_eq!(ids, &[1, 2]);
    handle_overlay_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &worker);
    assert!(app.overlay.is_none());
    assert!(matches!(rx.try_recv(), Ok(WorkerCmd::SidekickStop { spawn_id: 1 })));
}

#[test]
fn sidekick_stop_escape_cancels_and_empty_state_is_clear() {
    let (worker, mut rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    slash_command(&mut app, "sidekick stop", &worker, 80);
    assert!(last_notice(&app).contains("No active sidekicks."));
    let mut transcript = crate::tui::state::SubagentTranscript::new(1, None, 0, "call".into(), "task".into(), None);
    transcript.persistent = true;
    transcript.status = crate::tui::state::SubagentTranscriptStatus::Idle;
    app.subagent_transcripts.insert(1, transcript);
    slash_command(&mut app, "sidekick stop", &worker, 80);
    handle_overlay_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &worker);
    assert!(app.overlay.is_none());
    assert!(rx.try_recv().is_err());
}

#[test]
fn sidekick_result_is_idle_and_stop_retains_read_only_history() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_test_subagent(&mut app, &worker);
    handle_ui_msg(
        &mut app,
        UiMsg::Event(HarnessEvent::ToolResult {
            tool_call_id: "outer-7".into(),
            tool_name: "subagent".into(),
            output: serde_json::json!({
                "spawnId": 7,
                "status": "idle",
                "termination": "retained",
                "answer": "short report"
            }),
            is_error: false,
        }),
        &worker,
        100,
    );
    let transcript = app.subagent_transcripts.get(&7).unwrap();
    assert!(transcript.persistent);
    assert_eq!(
        transcript.status,
        crate::tui::state::SubagentTranscriptStatus::Idle
    );
    assert!(crate::tui::state::AgentTab::Done.admits(transcript.status));
    assert!(!crate::tui::state::AgentTab::Running.admits(transcript.status));

    handle_ui_msg(
        &mut app,
        UiMsg::Event(HarnessEvent::ToolResult {
            tool_call_id: "stop-7".into(),
            tool_name: "subagent".into(),
            output: serde_json::json!({
                "spawnId": 7,
                "status": "stopped",
                "termination": "stopped"
            }),
            is_error: false,
        }),
        &worker,
        100,
    );
    let transcript = app.subagent_transcripts.get(&7).unwrap();
    assert_eq!(
        transcript.status,
        crate::tui::state::SubagentTranscriptStatus::Stopped
    );
    assert!(crate::tui::state::AgentTab::Done.admits(transcript.status));
}

#[test]
fn sidekick_acknowledgements_move_the_same_row_between_running_and_done() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_test_subagent(&mut app, &worker);
    app.agent_browser = Some(crate::tui::state::AgentBrowser::new(1));

    // This is the actual background acknowledgement shape. It is also
    // `termination=detached`, which used to take the generic detached branch
    // and skip mark_sidekick_result entirely.
    handle_ui_msg(
        &mut app,
        UiMsg::Event(HarnessEvent::ToolResult {
            tool_call_id: "outer-7".into(),
            tool_name: "subagent".into(),
            output: serde_json::json!({
                "spawnId": 7, "status": "queued", "persistent": true,
                "termination": "detached"
            }),
            is_error: false,
        }),
        &worker,
        160,
    );
    assert_eq!(app.subagent_transcripts[&7].status, crate::tui::state::SubagentTranscriptStatus::Queued);
    assert_eq!(crate::tui::render::agent_ids(&app), vec![7]);

    finish_test_subagent(&mut app, &worker, 7, "outer-7", HarnessEvent::AgentStart);
    finish_test_subagent(
        &mut app,
        &worker,
        7,
        "outer-7",
        HarnessEvent::AssistantDelta { text: "first stream".into() },
    );
    assert_eq!(app.subagent_transcripts[&7].status, crate::tui::state::SubagentTranscriptStatus::Running);
    assert_eq!(crate::tui::render::agent_ids(&app), vec![7]);

    finish_test_subagent(
        &mut app,
        &worker,
        7,
        "outer-7",
        HarnessEvent::Result { message: "first complete".into() },
    );
    assert_eq!(app.subagent_transcripts[&7].status, crate::tui::state::SubagentTranscriptStatus::Idle);
    assert!(crate::tui::render::agent_ids(&app).is_empty());
    app.agent_browser.as_mut().unwrap().tab = crate::tui::state::AgentTab::Done;
    assert_eq!(crate::tui::render::agent_ids(&app), vec![7]);

    // A later `action=task` returns the same detached acknowledgement, now
    // for the retained handle. It must actively return this row to Running.
    app.agent_browser.as_mut().unwrap().tab = crate::tui::state::AgentTab::Running;
    handle_ui_msg(
        &mut app,
        UiMsg::Event(HarnessEvent::ToolResult {
            tool_call_id: "follow-up-7".into(),
            tool_name: "subagent".into(),
            output: serde_json::json!({
                "spawnId": 7, "status": "running", "persistent": true,
                "termination": "detached"
            }),
            is_error: false,
        }),
        &worker,
        160,
    );
    assert_eq!(app.subagent_transcripts[&7].status, crate::tui::state::SubagentTranscriptStatus::Running);
    assert_eq!(crate::tui::render::agent_ids(&app), vec![7]);
    finish_test_subagent(
        &mut app,
        &worker,
        7,
        "outer-7",
        HarnessEvent::AssistantDelta { text: "follow-up stream".into() },
    );
    finish_test_subagent(
        &mut app,
        &worker,
        7,
        "outer-7",
        HarnessEvent::Result { message: "follow-up complete".into() },
    );
    assert_eq!(app.subagent_transcripts[&7].status, crate::tui::state::SubagentTranscriptStatus::Idle);
    assert!(crate::tui::render::agent_ids(&app).is_empty());
    app.agent_browser.as_mut().unwrap().tab = crate::tui::state::AgentTab::Done;
    assert_eq!(crate::tui::render::agent_ids(&app), vec![7]);
}

#[test]
fn initial_sidekick_failure_is_failed_but_established_followup_failure_is_idle() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    handle_ui_msg(
        &mut app,
        UiMsg::Event(HarnessEvent::ToolCall {
            tool_call_id: "outer-7".into(),
            tool_name: "subagent".into(),
            input: serde_json::json!({"task": "first", "persistent": true}),
        }),
        &worker,
        100,
    );
    start_test_subagent(&mut app, &worker);

    finish_test_subagent(
        &mut app,
        &worker,
        7,
        "outer-7",
        HarnessEvent::Error {
            message: "initial failed".into(),
        },
    );
    assert!(app.subagent_transcripts[&7].persistent);
    assert_eq!(
        app.subagent_transcripts[&7].status,
        crate::tui::state::SubagentTranscriptStatus::Failed,
        "creation failed before the runtime returned a reusable handle"
    );

    handle_ui_msg(
        &mut app,
        UiMsg::Event(HarnessEvent::ToolResult {
            tool_call_id: "outer-7".into(),
            tool_name: "subagent".into(),
            output: serde_json::json!({"spawnId": 7, "termination": "retained"}),
            is_error: false,
        }),
        &worker,
        100,
    );
    finish_test_subagent(
        &mut app,
        &worker,
        7,
        "follow-up-7",
        HarnessEvent::Error {
            message: "follow-up failed".into(),
        },
    );
    assert_eq!(
        app.subagent_transcripts[&7].status,
        crate::tui::state::SubagentTranscriptStatus::Idle
    );

    start_named_test_subagent(&mut app, &worker, 8, None, 0, "one-shot", "ordinary");
    finish_test_subagent(
        &mut app,
        &worker,
        8,
        "one-shot",
        HarnessEvent::Error {
            message: "ordinary failure".into(),
        },
    );
    assert_eq!(
        app.subagent_transcripts[&8].status,
        crate::tui::state::SubagentTranscriptStatus::Failed
    );
}

#[test]
fn stopped_sidekick_keeps_late_error_and_result_without_leaving_stopped() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_test_subagent(&mut app, &worker);
    handle_ui_msg(
        &mut app,
        UiMsg::Event(HarnessEvent::ToolResult {
            tool_call_id: "stop-7".into(),
            tool_name: "subagent".into(),
            output: serde_json::json!({"spawnId": 7, "termination": "stopped"}),
            is_error: false,
        }),
        &worker,
        100,
    );
    for event in [
        HarnessEvent::Error {
            message: "late cancellation".into(),
        },
        HarnessEvent::Result {
            message: "late result".into(),
        },
    ] {
        finish_test_subagent(&mut app, &worker, 7, "outer-7", event);
        assert_eq!(
            app.subagent_transcripts[&7].status,
            crate::tui::state::SubagentTranscriptStatus::Stopped
        );
    }
    assert_eq!(app.subagent_transcripts[&7].latest_answer(), Some("late result"));
}

#[test]
fn queued_background_agent_reads_as_queued_until_its_inner_loop_starts() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_named_test_subagent(&mut app, &worker, 7, None, 0, "outer-7", "first");
    start_named_test_subagent(&mut app, &worker, 8, None, 0, "outer-8", "second");
    for (id, call_id, status) in [(7, "outer-7", "running"), (8, "outer-8", "queued")] {
        handle_ui_msg(
            &mut app,
            UiMsg::Event(HarnessEvent::ToolResult {
                tool_call_id: call_id.into(),
                tool_name: "subagent".into(),
                output: serde_json::json!({
                    "spawnId": id,
                    "status": status,
                    "termination": "detached"
                }),
                is_error: false,
            }),
            &worker,
            160,
        );
    }
    // Only the first worker's inner loop has started.
    handle_ui_msg(
        &mut app,
        UiMsg::SubagentEvent {
            id: 7,
            parent_id: None,
            depth: 0,
            call_id: "outer-7".into(),
            event: HarnessEvent::AgentStart,
        },
        &worker,
        160,
    );

    let first = app.subagent_transcripts.get(&7).unwrap();
    let second = app.subagent_transcripts.get(&8).unwrap();
    assert_eq!(
        first.status,
        crate::tui::state::SubagentTranscriptStatus::Running
    );
    assert_eq!(
        second.status,
        crate::tui::state::SubagentTranscriptStatus::Queued
    );
    assert!(second.detached, "a queued spawn is still a detached one");
    assert!(
        app.subagent_activity.contains_key(&8),
        "queued is not terminal: the activity record must survive"
    );

    app.agent_browser = Some(crate::tui::state::AgentBrowser::new(2));
    app.agent_browser.as_mut().unwrap().picker.move_by(1);
    let rendered = rendered_rows(&mut app, 160, 36).join("\n");
    assert!(rendered.contains("queued ·"), "{rendered}");
    assert!(rendered.contains("active 1"), "{rendered}");
    assert!(rendered.contains("queued 1"), "{rendered}");
    assert!(rendered.contains("Waiting for a slot"), "{rendered}");

    // Its slot arrives: the first event from the inner loop flips it.
    handle_ui_msg(
        &mut app,
        UiMsg::SubagentEvent {
            id: 8,
            parent_id: None,
            depth: 0,
            call_id: "outer-8".into(),
            event: HarnessEvent::AgentStart,
        },
        &worker,
        160,
    );
    assert_eq!(
        app.subagent_transcripts.get(&8).unwrap().status,
        crate::tui::state::SubagentTranscriptStatus::Running
    );
    let rendered = rendered_rows(&mut app, 160, 36).join("\n");
    assert!(rendered.contains("active 2"), "{rendered}");
    assert!(!rendered.contains("queued"), "{rendered}");
}

#[test]
fn down_enters_the_status_row_and_arrows_reach_agents() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_test_subagent(&mut app, &worker);

    let initial = rendered_rows(&mut app, 120, 30).join("\n");
    assert!(initial.contains("agents 1 ↓"), "{initial}");

    handle_terminal_event(
        &mut app,
        CtEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &worker,
        120,
    );
    assert_eq!(app.status_focus, Some(StatusFocus::Context));
    handle_terminal_event(
        &mut app,
        CtEvent::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
        &worker,
        120,
    );
    assert_eq!(app.status_focus, Some(StatusFocus::Agents));
    let status = rendered_rows(&mut app, 120, 30).join("\n");
    assert!(status.contains("agents 1"), "{status}");
    assert!(status.contains("enter open"), "{status}");

    handle_terminal_event(
        &mut app,
        CtEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &worker,
        120,
    );
    assert!(app.agent_browser.is_some());

    handle_ui_msg(
        &mut app,
        UiMsg::SubagentEvent {
            id: 7,
            parent_id: None,
            depth: 0,
            call_id: "outer-7".into(),
            event: HarnessEvent::AssistantDelta {
                text: "live transcript text".into(),
            },
        },
        &worker,
        120,
    );
    let browser = rendered_rows(&mut app, 120, 30).join("\n");
    assert!(browser.contains("inspect queue ordering"), "{browser}");
    assert!(browser.contains("live transcript text"), "{browser}");
    assert!(browser.contains("parent continues"), "{browser}");

    handle_terminal_event(
        &mut app,
        CtEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &worker,
        120,
    );
    assert!(app.agent_browser.is_none());
}

#[test]
fn status_line_counts_only_running_and_queued_agents() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = browser_with_one_agent_per_state(&worker);
    app.agent_browser = None;
    start_named_test_subagent(&mut app, &worker, 10, None, 0, "queued", "queued task");
    finish_test_subagent(&mut app, &worker, 7, "active", HarnessEvent::AgentStart);
    let rendered = rendered_rows(&mut app, 160, 36).join("\n");
    assert!(rendered.contains("agents 2 ↓"), "{rendered}");
    for (id, call_id) in [(7, "active"), (10, "queued")] {
        finish_test_subagent(
            &mut app,
            &worker,
            id,
            call_id,
            HarnessEvent::Result {
                message: "done".into(),
            },
        );
    }
    let rendered = rendered_rows(&mut app, 160, 36).join("\n");
    assert!(
        !rendered.lines().last().unwrap().contains("agents "),
        "{rendered}"
    );
    assert_eq!(
        app.subagent_transcripts.len(),
        4,
        "history remains available"
    );
    press_in_browser(&mut app, &worker, KeyCode::Down);
    assert_eq!(app.status_focus, Some(StatusFocus::Context));
    press_in_browser(&mut app, &worker, KeyCode::Right);
    assert_eq!(app.status_focus, Some(StatusFocus::Agents));
    press_in_browser(&mut app, &worker, KeyCode::Enter);
    assert!(app.agent_browser.is_some());
}

#[test]
fn selected_agent_uses_the_normal_thinking_and_work_rails() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_test_subagent(&mut app, &worker);
    app.agent_browser = Some(crate::tui::state::AgentBrowser::new(1));
    app.agent_browser.as_mut().unwrap().tab = crate::tui::state::AgentTab::All;

    for event in [
        HarnessEvent::ReasoningDelta {
            text: "inspect the file".into(),
        },
        HarnessEvent::ToolCall {
            tool_call_id: "read-1".into(),
            tool_name: "read_file".into(),
            input: serde_json::json!({"path": "README.md"}),
        },
        HarnessEvent::ToolStarted {
            tool_call_id: "read-1".into(),
            tool_name: "read_file".into(),
        },
        HarnessEvent::ToolFinished {
            tool_call_id: "read-1".into(),
            tool_name: "read_file".into(),
            is_error: false,
        },
        HarnessEvent::ToolResult {
            tool_call_id: "read-1".into(),
            tool_name: "read_file".into(),
            output: serde_json::json!({"content": "hello"}),
            is_error: false,
        },
        HarnessEvent::Result {
            message: "finished inspection".into(),
        },
    ] {
        handle_ui_msg(
            &mut app,
            UiMsg::SubagentEvent {
                id: 7,
                parent_id: None,
                depth: 0,
                call_id: "outer-7".into(),
                event,
            },
            &worker,
            160,
        );
    }

    let browser = rendered_rows(&mut app, 160, 36).join("\n");
    assert!(browser.contains("Thinking"), "{browser}");
    assert!(browser.contains("Work"), "{browser}");
    assert!(browser.contains("Read · README.md"), "{browser}");
    assert!(browser.contains("finished inspection"), "{browser}");
}

#[test]
fn down_keeps_history_behavior_when_the_composer_has_a_draft() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_test_subagent(&mut app, &worker);
    app.composer = "unfinished prompt".into();
    app.cursor = app.composer.chars().count();

    handle_terminal_event(
        &mut app,
        CtEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &worker,
        100,
    );

    assert!(app.status_focus.is_none());
    assert_eq!(app.composer, "unfinished prompt");
}

#[test]
fn agent_browser_routes_mouse_and_ignores_composer_paste() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_test_subagent(&mut app, &worker);
    app.agent_browser = Some(crate::tui::state::AgentBrowser::new(1));
    handle_terminal_event(
        &mut app,
        CtEvent::Paste("hidden draft".into()),
        &worker,
        160,
    );
    assert!(app.composer.is_empty());
    handle_terminal_event(
        &mut app,
        CtEvent::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }),
        &worker,
        160,
    );
    assert_eq!(app.agent_browser.as_ref().unwrap().scroll, 3);
    assert_eq!(app.scroll, 0);
}

#[test]
fn nested_background_agent_is_detached_and_releases_live_activity() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_test_subagent(&mut app, &worker);
    start_named_test_subagent(&mut app, &worker, 8, Some(7), 1, "nested", "nested task");
    finish_test_subagent(
        &mut app,
        &worker,
        7,
        "outer-7",
        HarnessEvent::ToolResult {
            tool_call_id: "nested".into(),
            tool_name: "subagent".into(),
            output: serde_json::json!({"termination": "detached", "spawnId": 8}),
            is_error: false,
        },
    );
    assert!(app.subagent_transcripts[&8].detached);
    finish_test_subagent(
        &mut app,
        &worker,
        8,
        "nested",
        HarnessEvent::Result {
            message: "nested answer".into(),
        },
    );
    assert!(!app.subagent_activity.contains_key(&8));
    assert_eq!(
        app.subagent_transcripts[&8].latest_answer(),
        Some("nested answer")
    );
}

#[test]
fn queued_background_completion_marks_failure_and_deduplicates_observer_results() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_test_subagent(&mut app, &worker);
    app.subagent_transcripts.get_mut(&7).unwrap().detached = true;
    handle_ui_msg(
        &mut app,
        UiMsg::SubagentCompleted {
            id: 7,
            is_error: true,
            message: "cancelled before acquiring slot".into(),
        },
        &worker,
        160,
    );
    assert_eq!(
        app.subagent_transcripts[&7].status,
        crate::tui::state::SubagentTranscriptStatus::Failed
    );
    assert!(!app.subagent_activity.contains_key(&7));
    let count = app.subagent_transcripts[&7].entries.len();
    handle_ui_msg(
        &mut app,
        UiMsg::SubagentCompleted {
            id: 7,
            is_error: true,
            message: "duplicate failure".into(),
        },
        &worker,
        160,
    );
    assert_eq!(app.subagent_transcripts[&7].entries.len(), count);
}
