// The workflow graph panel: every submitted stage is visible from the
// acknowledgement, before any of them spawns.

use crate::tui::state::workflow::StageState;

const WORKFLOW_GRAPH: &str = r#"[
    {"id":"discover","prompt":"list every *.ts file"},
    {"id":"count_src","needs":["discover"],"prompt":"count under src/"},
    {"id":"count_test","needs":["discover"],"prompt":"count under tests/"},
    {"id":"summary","needs":["count_src","count_test"],"prompt":"summarise"}
]"#;

/// Submit the demo graph and acknowledge it, exactly as the harness does:
/// a tool call carrying the graph, then a detached result naming the run.
fn submit_workflow(app: &mut App, run: u64) {
    let graph: serde_json::Value = serde_json::from_str(WORKFLOW_GRAPH).unwrap();
    handle_harness_event(
        app,
        HarnessEvent::ToolCall {
            tool_call_id: "wf-call".into(),
            tool_name: "workflow".into(),
            input: serde_json::json!({"action":"run","graph":graph}),
        },
        TEST_TERMINAL_WIDTH,
    );
    handle_harness_event(
        app,
        HarnessEvent::ToolResult {
            tool_call_id: "wf-call".into(),
            tool_name: "workflow".into(),
            output: serde_json::json!({
                "runId": run,
                "status": "running",
                "stages": 4,
                "termination": "detached",
            }),
            is_error: false,
        },
        TEST_TERMINAL_WIDTH,
    );
}

fn stage_states(app: &App, run: u64) -> Vec<(String, StageState)> {
    app.workflows[&run]
        .stages
        .iter()
        .map(|stage| (stage.id.clone(), stage.state))
        .collect()
}

#[test]
fn the_whole_graph_is_known_before_any_stage_spawns() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_named_test_subagent(&mut app, &worker, 412, None, 0, "wf-call", "workflow · 4 stages");
    submit_workflow(&mut app, 412);

    assert_eq!(
        stage_states(&app, 412),
        [
            ("discover".to_string(), StageState::Blocked),
            ("count_src".to_string(), StageState::Blocked),
            ("count_test".to_string(), StageState::Blocked),
            ("summary".to_string(), StageState::Blocked),
        ],
        "every submitted stage exists with no agent spawned"
    );
    let details: Vec<_> = app.workflows[&412]
        .stages
        .iter()
        .map(|stage| stage.detail.clone().unwrap_or_default())
        .collect();
    assert_eq!(details[0], "queued", "a root stage waits on nothing");
    assert_eq!(details[1], "blocked on discover");
    assert_eq!(details[3], "blocked on count_src +1");
}

#[test]
fn a_finished_dependency_clears_the_reason_it_blocked() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_named_test_subagent(&mut app, &worker, 412, None, 0, "wf-call", "workflow · 4 stages");
    submit_workflow(&mut app, 412);

    handle_ui_msg(
        &mut app,
        UiMsg::SubagentStarted {
            run: Some(412),
            stage: Some("discover".into()),
            id: 413,
            parent_id: Some(412),
            depth: 1,
            call_id: "wf-call".into(),
            task: "list every *.ts file".into(),
            identity: None,
        },
        &worker,
        100,
    );
    assert_eq!(app.workflows[&412].stages[0].state, StageState::Queued);
    assert_eq!(app.workflow_stages[&413], (412, 0));

    finish_test_subagent(
        &mut app,
        &worker,
        413,
        "wf-call",
        HarnessEvent::Result {
            message: "6 files".into(),
        },
    );
    let run = &app.workflows[&412];
    assert_eq!(run.stages[0].state, StageState::Done);
    assert_eq!(
        run.stages[1].detail.as_deref(),
        Some("queued"),
        "count_src no longer blocks once discover is done"
    );
    assert_eq!(
        run.stages[3].detail.as_deref(),
        Some("blocked on count_src +1"),
        "summary still names what it waits on"
    );
}

#[test]
fn a_run_outcome_settles_stages_that_never_reported() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_named_test_subagent(&mut app, &worker, 412, None, 0, "wf-call", "workflow · 4 stages");
    submit_workflow(&mut app, 412);
    handle_ui_msg(
        &mut app,
        UiMsg::SubagentCompleted {
            id: 412,
            is_error: true,
            message: format!(
                "workflow failed (412): count_src: model error 429\n{}",
                serde_json::json!({
                    "state": "failed",
                    "error": "count_src: model error 429",
                    "stages": {
                        "discover": "done",
                        "count_src": "failed",
                        "count_test": "stopped",
                        "summary": "stopped",
                    },
                })
            ),
        },
        &worker,
        100,
    );
    assert_eq!(
        stage_states(&app, 412),
        [
            ("discover".to_string(), StageState::Done),
            ("count_src".to_string(), StageState::Failed),
            ("count_test".to_string(), StageState::Stopped),
            ("summary".to_string(), StageState::Stopped),
        ],
        "the run's own report is authoritative for stages that never spoke"
    );
    assert_eq!(
        app.workflows[&412].error.as_deref(),
        Some("count_src: model error 429")
    );
}

/// The panel draws in both styles, and neither invents a mark: every state
/// glyph on screen comes from the active table.
fn workflow_panel(app: &mut App, style: crate::view::glyphs::UiStyle) -> String {
    crate::view::glyphs::set_ui_style(style);
    let rows = crate::tui::render::agent_tree_rows(app);
    let mut browser = crate::tui::state::AgentBrowser::new(rows.len());
    // The All tab, so one helper serves a live run and a finished one.
    browser.tab = crate::tui::state::AgentTab::All;
    app.agent_browser = Some(browser);
    rendered_rows(app, 160, 24).join("\n")
}

#[test]
fn the_panel_renders_the_graph_in_both_styles() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_named_test_subagent(&mut app, &worker, 412, None, 0, "wf-call", "workflow · 4 stages");
    submit_workflow(&mut app, 412);

    for style in crate::view::glyphs::UiStyle::ALL {
        let rendered = workflow_panel(&mut app, style);
        let g = style.glyphs();
        assert!(
            rendered.contains("Workflow #412 · 4 stages"),
            "{style:?}: {rendered}"
        );
        assert!(rendered.contains("0/4 done"), "{style:?}: {rendered}");
        for stage in ["discover", "count_src", "count_test", "summary"] {
            assert!(
                rendered.contains(&format!("Stage · {stage}")),
                "{style:?} is missing {stage}: {rendered}"
            );
        }
        assert!(
            rendered.contains("blocked on discover"),
            "{style:?}: {rendered}"
        );
        assert!(
            !rendered.contains('◐') && !rendered.contains('⊘'),
            "{style:?} drew a mark outside the glyph table: {rendered}"
        );
        // Nothing has run, so no row may carry a finished or failed mark.
        let body: String = rendered
            .lines()
            .filter(|line| line.contains("Stage · "))
            .collect();
        assert!(
            !body.contains(g.done) && !body.contains(g.failed),
            "{style:?} marked an unstarted stage: {body}"
        );
    }
    crate::view::glyphs::set_ui_style(crate::view::glyphs::UiStyle::Minimal);
}

#[test]
fn finished_and_failed_stages_take_their_marks_from_the_table() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_named_test_subagent(&mut app, &worker, 412, None, 0, "wf-call", "workflow · 4 stages");
    submit_workflow(&mut app, 412);
    handle_ui_msg(
        &mut app,
        UiMsg::SubagentCompleted {
            id: 412,
            is_error: true,
            message: format!(
                "workflow failed (412): count_src: model error 429\n{}",
                serde_json::json!({
                    "state": "failed",
                    "error": "count_src: model error 429",
                    "stages": {
                        "discover": "done",
                        "count_src": "failed",
                        "count_test": "stopped",
                        "summary": "stopped",
                    },
                })
            ),
        },
        &worker,
        100,
    );
    for style in crate::view::glyphs::UiStyle::ALL {
        let rendered = workflow_panel(&mut app, style);
        let g = style.glyphs();
        assert!(
            rendered.contains(&format!("{} Stage · discover", g.done)),
            "{style:?}: {rendered}"
        );
        assert!(
            rendered.contains(&format!("{} Stage · count_src", g.failed)),
            "{style:?}: {rendered}"
        );
        assert!(
            rendered.contains("- Stage · count_test"),
            "a stopped stage keeps the unstarted mark: {rendered}"
        );
        assert!(
            rendered.contains("1/4 done · 1 failed · 2 stopped"),
            "{style:?}: {rendered}"
        );
        assert!(
            rendered.contains("count_src: model error 429"),
            "the run's cause is on screen: {rendered}"
        );
    }
    crate::view::glyphs::set_ui_style(crate::view::glyphs::UiStyle::Minimal);
}
