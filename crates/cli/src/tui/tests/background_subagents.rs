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
fn down_focuses_agents_and_enter_opens_the_live_browser() {
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
    assert!(app.agents_status_focused);
    let status = rendered_rows(&mut app, 120, 30).join("\n");
    assert!(status.contains("agents 1"), "{status}");
    assert!(status.contains("enter open agents"), "{status}");

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
    assert!(app.agents_status_focused);
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

    assert!(!app.agents_status_focused);
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
