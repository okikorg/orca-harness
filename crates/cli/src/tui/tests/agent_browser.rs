use crate::tui::render::agent_tree_rows;

fn start_test_subagent(app: &mut App, worker: &mpsc::UnboundedSender<WorkerCmd>) {
    start_named_test_subagent(app, worker, 7, None, 0, "outer-7", "inspect queue ordering");
}

fn start_named_test_subagent(
    app: &mut App,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    id: u64,
    parent_id: Option<u64>,
    depth: u32,
    call_id: &str,
    task: &str,
) {
    handle_ui_msg(
        app,
        UiMsg::SubagentStarted {
            run: None,
            stage: None,
            id,
            parent_id,
            depth,
            call_id: call_id.into(),
            task: task.into(),
            identity: Some(orca_harness_tools::SubagentIdentity::new(
                "test",
                "worker-model",
            )),
        },
        worker,
        100,
    );
}

#[test]
fn agent_browser_orders_concurrent_spawns_as_parent_linked_trees() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_named_test_subagent(&mut app, &worker, 10, None, 0, "root-a", "root a");
    start_named_test_subagent(&mut app, &worker, 20, None, 0, "root-b", "root b");
    start_named_test_subagent(&mut app, &worker, 30, Some(10), 1, "child-a", "child a");
    start_named_test_subagent(
        &mut app,
        &worker,
        40,
        Some(30),
        2,
        "grandchild-a",
        "grandchild a",
    );
    start_named_test_subagent(&mut app, &worker, 50, Some(10), 1, "child-b", "child b");

    let rows = agent_tree_rows(&app);
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [10, 30, 40, 50, 20]
    );
    assert_eq!(rows[0].prefix, "");
    assert_eq!(rows[1].prefix, "├─ ");
    assert_eq!(rows[2].prefix, "│  └─ ");
    assert_eq!(rows[3].prefix, "└─ ");
    assert_eq!(rows[4].prefix, "");

    app.agent_browser = Some(crate::tui::state::AgentBrowser::new(rows.len()));
    let rendered = rendered_rows(&mut app, 160, 36).join("\n");
    assert!(rendered.contains("- #10 root a"), "{rendered}");
    assert!(rendered.lines().any(|line| line.contains("#10 root a") && line.contains("queued ·")), "{rendered}");
    assert!(rendered.contains("├─ - #30 child a"), "{rendered}");
    assert!(rendered.contains("│  └─ - #40 grandchild a"), "{rendered}");
    assert!(rendered.contains("└─ - #50 child b"), "{rendered}");
}

fn finish_test_subagent(
    app: &mut App,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    id: u64,
    call_id: &str,
    event: HarnessEvent,
) {
    handle_ui_msg(
        app,
        UiMsg::SubagentEvent {
            id,
            parent_id: None,
            depth: 0,
            call_id: call_id.into(),
            event,
        },
        worker,
        160,
    );
}

/// One running, one finished and one failed agent, with the browser open
/// on its default tab.
fn browser_with_one_agent_per_state(worker: &mpsc::UnboundedSender<WorkerCmd>) -> App {
    let mut app = test_app();
    start_named_test_subagent(&mut app, worker, 7, None, 0, "active", "active task");
    start_named_test_subagent(&mut app, worker, 8, None, 0, "done", "finished task");
    start_named_test_subagent(&mut app, worker, 9, None, 0, "failed", "broken task");
    finish_test_subagent(
        &mut app,
        worker,
        8,
        "done",
        HarnessEvent::Result {
            message: "finished".into(),
        },
    );
    finish_test_subagent(
        &mut app,
        worker,
        9,
        "failed",
        HarnessEvent::Error {
            message: "boom".into(),
        },
    );
    app.agent_browser = Some(crate::tui::state::AgentBrowser::new(3));
    app
}

fn press_in_browser(app: &mut App, worker: &mpsc::UnboundedSender<WorkerCmd>, code: KeyCode) {
    handle_terminal_event(
        app,
        CtEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
        worker,
        160,
    );
}

#[test]
fn agent_browser_tab_cycles_running_done_failed_and_all() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = browser_with_one_agent_per_state(&worker);

    let visible = |app: &mut App| {
        let rows = rendered_rows(app, 160, 36).join("\n");
        ["active task", "finished task", "broken task"].map(|task| rows.contains(task))
    };

    let rendered = rendered_rows(&mut app, 160, 36).join("\n");
    assert!(
        rendered.contains("Running 1 · Done 1 · Failed 1 · All 3"),
        "{rendered}"
    );
    assert!(!rendered.contains("Done / stalled / failed"), "{rendered}");
    assert_eq!(visible(&mut app), [true, false, false], "running tab");
    assert!(rendered_cell_is_bold(&mut app, 160, 36, "Running"));
    assert!(!rendered_cell_is_bold(&mut app, 160, 36, "All"));

    press_in_browser(&mut app, &worker, KeyCode::Tab);
    assert_eq!(visible(&mut app), [false, true, false], "done tab");
    assert!(rendered_cell_is_bold(&mut app, 160, 36, "Done"));

    press_in_browser(&mut app, &worker, KeyCode::Tab);
    assert_eq!(visible(&mut app), [false, false, true], "failed tab");
    assert!(rendered_cell_is_bold(&mut app, 160, 36, "Failed"));

    press_in_browser(&mut app, &worker, KeyCode::Tab);
    assert_eq!(visible(&mut app), [true, true, true], "all tab");
    assert!(rendered_cell_is_bold(&mut app, 160, 36, "All"));

    press_in_browser(&mut app, &worker, KeyCode::Tab);
    assert_eq!(visible(&mut app), [true, false, false], "back to running");
    assert!(app.agent_browser.is_some(), "tab never closes the browser");

    press_in_browser(&mut app, &worker, KeyCode::Tab);
    press_in_browser(&mut app, &worker, KeyCode::Esc);
    app.agents_status_focused = true;
    press_in_browser(&mut app, &worker, KeyCode::Enter);
    assert_eq!(visible(&mut app), [true, false, false], "reopens on running");
    assert!(rendered_cell_is_bold(&mut app, 160, 36, "Running"));
}

#[test]
fn agent_browser_tab_keeps_the_cursor_on_the_same_agent_when_it_stays_visible() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = browser_with_one_agent_per_state(&worker);
    app.agent_browser.as_mut().unwrap().tab = crate::tui::state::AgentTab::All;
    rendered_rows(&mut app, 160, 36);
    press_in_browser(&mut app, &worker, KeyCode::Down);
    press_in_browser(&mut app, &worker, KeyCode::Down);
    assert_eq!(app.agent_browser.as_ref().unwrap().picker.index(), 2);

    // Running, Done, Failed: the failed agent is the only row on its tab.
    for _ in 0..3 {
        press_in_browser(&mut app, &worker, KeyCode::Tab);
        rendered_rows(&mut app, 160, 36);
    }
    assert_eq!(app.agent_browser.as_ref().unwrap().picker.index(), 0);
    let rendered = rendered_rows(&mut app, 160, 36).join("\n");
    assert!(rendered.contains("broken task"), "{rendered}");

    // Back on All, the failed agent is still the selected row: third of three.
    press_in_browser(&mut app, &worker, KeyCode::Tab);
    assert_eq!(app.agent_browser.as_ref().unwrap().picker.index(), 2);
}

#[test]
fn agent_page_renders_task_first_entries_counts_and_empty_filters() {
    let _theme = THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
    let original = view::theme_name();
    view::set_theme(view::ThemeName::Orca);
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    set_ui_style(UiStyle::Glyph);
    start_named_test_subagent(&mut app, &worker, 12, None, 0, "root-12", "Review dispatcher and trace cancellation through queued tool calls");
    start_named_test_subagent(&mut app, &worker, 15, Some(12), 1, "child-15", "Check cancellation");
    start_named_test_subagent(&mut app, &worker, 11, None, 0, "root-11", "Inspect documentation");
    finish_test_subagent(&mut app, &worker, 11, "root-11", HarnessEvent::Result { message: "Documentation matches the current behavior.".into() });
    finish_test_subagent(&mut app, &worker, 12, "root-12", HarnessEvent::ReasoningDelta { text: "Tracing how queued calls react to cancellation.".into() });
    finish_test_subagent(&mut app, &worker, 12, "root-12", HarnessEvent::ToolCall { tool_call_id: "read-12".into(), tool_name: "read_file".into(), input: serde_json::json!({"path": "crates/harness-core/src/dispatcher.rs"}) });
    finish_test_subagent(&mut app, &worker, 12, "root-12", HarnessEvent::ToolResult { tool_call_id: "read-12".into(), tool_name: "read_file".into(), output: serde_json::json!({"bytes": 20600}), is_error: false });
    finish_test_subagent(&mut app, &worker, 12, "root-12", HarnessEvent::AssistantDelta { text: "Cancellation reaches active tools. I am checking queued calls next.".into() });
    app.subagent_transcripts.get_mut(&12).unwrap().input_tokens = 8200;
    app.subagent_transcripts.get_mut(&12).unwrap().output_tokens = 1100;
    app.agent_browser = Some(crate::tui::state::AgentBrowser::new(3));
    for width in [60, 100, 160] {
        let rows = rendered_rows(&mut app, width, 30);
        let screen = rows.join("\n");
        assert!(screen.contains("Agents · 3 total"), "{screen}");
        assert!(screen.contains("#12"), "{screen}");
        assert!(screen.contains("Review dispatcher"), "{screen}");
        assert!(screen.contains("Tokens: in 8.2k"), "{screen}");
        assert!(screen.contains("tab"), "{screen}");
        println!("\n--- Orca agent page / {width} columns ---\n{screen}");
    }
    app.agent_browser.as_mut().unwrap().tab = crate::tui::state::AgentTab::Failed;
    let empty = rendered_rows(&mut app, 100, 30).join("\n");
    assert!(empty.contains("No failed agents"), "{empty}");
    assert!(empty.contains("1 completed"), "{empty}");
    finish_test_subagent(&mut app, &worker, 12, "root-12", HarnessEvent::Result { message: "Review complete.".into() });
    finish_test_subagent(&mut app, &worker, 15, "child-15", HarnessEvent::Result { message: "Cancellation checked.".into() });
    app.agent_browser.as_mut().unwrap().tab = crate::tui::state::AgentTab::Running;
    let idle = rendered_rows(&mut app, 100, 30).join("\n");
    assert!(idle.contains("No running agents"), "{idle}");
    assert!(idle.contains("3 completed"), "{idle}");
    view::set_theme(original);
    set_ui_style(UiStyle::Minimal);
}
