#[test]
fn agent_history_evicts_old_completions_but_preserves_active_children() {
    use crate::tui::state::subagent_history::COMPLETED_AGENT_HISTORY;
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_named_test_subagent(&mut app, &worker, 1, None, 0, "parent", "old parent");
    start_named_test_subagent(&mut app, &worker, 2, Some(1), 1, "child", "active child");
    finish_test_subagent(
        &mut app,
        &worker,
        1,
        "parent",
        HarnessEvent::Result {
            message: "done".into(),
        },
    );
    for id in 3..3 + COMPLETED_AGENT_HISTORY as u64 {
        start_named_test_subagent(&mut app, &worker, id, None, 0, "recent", "recent task");
        finish_test_subagent(
            &mut app,
            &worker,
            id,
            "recent",
            HarnessEvent::Result {
                message: "done".into(),
            },
        );
    }
    assert!(app.subagent_transcripts.contains_key(&1));
    assert!(app.subagent_transcripts[&2].status.is_active());
    assert_eq!(app.subagent_transcripts.len(), COMPLETED_AGENT_HISTORY + 2);
    assert_eq!(app.evicted_agent_histories, 0);
    let rows = agent_tree_rows(&app);
    assert_eq!(rows[0].id, 1);
    assert_eq!(rows[0].prefix, "", "an active tree retains its root");
    app.agent_browser = Some(crate::tui::state::AgentBrowser::new(rows.len()));
    let rendered = rendered_rows(&mut app, 180, 36).join("\n");
    assert!(rendered.contains("active child"), "{rendered}");
    assert!(!rendered.contains("older agents omitted"), "{rendered}");
}

#[test]
fn subagent_activity_compaction_preserves_out_of_order_results_and_pending_indices() {
    use crate::tui::state::subagent_history::{DISPLAY_TEXT_BYTES, SETTLED_ACTIVITY_HISTORY};
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_test_subagent(&mut app, &worker);
    let mut send = |event| finish_test_subagent(&mut app, &worker, 7, "outer-7", event);
    send(HarnessEvent::ToolCall {
        tool_call_id: "slow".into(),
        tool_name: "read".into(),
        input: serde_json::json!({}),
    });
    for index in 0..100 {
        let id = format!("fast-{index}");
        send(HarnessEvent::ToolCall {
            tool_call_id: id.clone(),
            tool_name: "read".into(),
            input: serde_json::json!({"text": "é".repeat(DISPLAY_TEXT_BYTES)}),
        });
        send(HarnessEvent::ToolResult {
            tool_call_id: id,
            tool_name: "read".into(),
            output: serde_json::json!({"text": "é".repeat(DISPLAY_TEXT_BYTES)}),
            is_error: false,
        });
    }
    let transcript = &app.subagent_transcripts[&7];
    assert_eq!(
        transcript.activity_tools.len(),
        SETTLED_ACTIVITY_HISTORY + 1
    );
    assert_eq!(transcript.pending_calls.len(), 1);
    assert_eq!(
        transcript.activity_tools[transcript.pending_calls["slow"]].call_id,
        "slow"
    );
    assert!(transcript.omitted_activity > 0);
    assert!(transcript
        .activity_tools
        .last()
        .unwrap()
        .output
        .as_ref()
        .unwrap()
        .as_str()
        .unwrap()
        .contains("history truncated"));
    assert_eq!(
        app.subagent_activity[&7].tools.len(),
        SETTLED_ACTIVITY_HISTORY + 1
    );

    finish_test_subagent(
        &mut app,
        &worker,
        7,
        "outer-7",
        HarnessEvent::Assistant {
            message: "still waiting".into(),
        },
    );
    assert_eq!(app.subagent_transcripts[&7].pending_calls["slow"], 0);
    finish_test_subagent(
        &mut app,
        &worker,
        7,
        "outer-7",
        HarnessEvent::ToolResult {
            tool_call_id: "slow".into(),
            tool_name: "read".into(),
            output: serde_json::json!("late result"),
            is_error: false,
        },
    );
    let transcript = &app.subagent_transcripts[&7];
    assert!(transcript.pending_calls.is_empty());
    assert_eq!(
        transcript.activity_tools[0].output,
        Some(serde_json::json!("late result"))
    );
    app.agent_browser = Some(crate::tui::state::AgentBrowser::new(1));
    let rendered = rendered_rows(&mut app, 180, 60).join("\n");
    assert!(
        rendered.contains("earlier history items omitted"),
        "{rendered}"
    );
}

#[test]
fn agent_browser_reuses_tree_counts_and_terminal_table_until_state_changes() {
    let _theme = THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_test_subagent(&mut app, &worker);
    finish_test_subagent(
        &mut app,
        &worker,
        7,
        "outer-7",
        HarnessEvent::Result {
            message: "ready".into(),
        },
    );
    app.agent_browser = Some(crate::tui::state::AgentBrowser::new(1));
    app.agent_browser.as_mut().unwrap().tab = crate::tui::state::AgentTab::All;
    rendered_rows(&mut app, 160, 36);
    let projection = app.agent_list_cache.borrow().as_ref().unwrap().clone();
    let table = app
        .agent_browser
        .as_ref()
        .unwrap()
        .table_cache
        .as_ref()
        .unwrap()
        .rows
        .clone();
    rendered_rows(&mut app, 160, 36);
    assert!(std::sync::Arc::ptr_eq(
        &projection,
        app.agent_list_cache.borrow().as_ref().unwrap()
    ));
    assert!(std::sync::Arc::ptr_eq(
        &table,
        &app.agent_browser
            .as_ref()
            .unwrap()
            .table_cache
            .as_ref()
            .unwrap()
            .rows
    ));
    start_named_test_subagent(&mut app, &worker, 8, None, 0, "new", "fresh queued task");
    assert!(app.agent_list_cache.borrow().is_none());
    let rendered = rendered_rows(&mut app, 160, 36).join("\n");
    assert!(rendered.contains("fresh queued task"), "{rendered}");
    assert!(rendered.contains("queued 1"), "{rendered}");
    finish_test_subagent(
        &mut app,
        &worker,
        8,
        "new",
        HarnessEvent::Error {
            message: "failed task".into(),
        },
    );
    let rendered = rendered_rows(&mut app, 160, 36).join("\n");
    assert!(rendered.contains("failed 1"), "{rendered}");
    assert!(!std::sync::Arc::ptr_eq(
        &projection,
        app.agent_list_cache.borrow().as_ref().unwrap()
    ));
}

#[test]
fn subagent_reasoning_and_streaming_previews_have_independent_budgets() {
    use crate::tui::state::subagent_history::{
        append_display_text, DISPLAY_TEXT_BYTES, THINKING_HISTORY,
    };
    let mut transcript =
        crate::tui::state::SubagentTranscript::new(1, None, 0, "call".into(), "task".into(), None);
    for _ in 0..THINKING_HISTORY * 3 {
        transcript.streaming_reasoning = "thinking".into();
        transcript.flush_reasoning();
    }
    assert_eq!(transcript.thinking_log.len(), THINKING_HISTORY);
    assert_eq!(transcript.omitted_activity, THINKING_HISTORY * 2);
    for _ in 0..100 {
        append_display_text(
            &mut transcript.streaming_assistant,
            &"é".repeat(DISPLAY_TEXT_BYTES),
        );
    }
    assert!(transcript.streaming_assistant.len() < DISPLAY_TEXT_BYTES + 64);
    assert!(transcript
        .streaming_assistant
        .ends_with("[history truncated]"));
}

#[test]
fn subagent_history_bounds_aggregate_payload_and_keeps_latest_answer() {
    use crate::tui::state::subagent_history::{DISPLAY_TEXT_BYTES, TRANSCRIPT_HISTORY_BYTES};
    use crate::tui::state::{SubagentTranscript, SubagentTranscriptEntry};
    let mut transcript = SubagentTranscript::new(1, None, 0, "call".into(), "task".into(), None);
    for batch in 0..20 {
        let tools = (0..32)
            .map(|index| {
                let mut tool = ToolActivity::new(
                    format!("{batch}-{index}"),
                    "read".into(),
                    serde_json::json!("i".repeat(DISPLAY_TEXT_BYTES)),
                );
                tool.record_result(serde_json::json!("o".repeat(DISPLAY_TEXT_BYTES)), false);
                tool
            })
            .collect();
        transcript.push_entry(SubagentTranscriptEntry::Activity {
            thinking: Vec::new(),
            tools,
        });
        assert!(transcript.retained_history_bytes <= TRANSCRIPT_HISTORY_BYTES);
        assert!(
            !transcript.entries.is_empty(),
            "oversized batches keep recent tools"
        );
    }
    transcript.push_assistant("latest answer".into());
    assert!(transcript.retained_history_bytes <= TRANSCRIPT_HISTORY_BYTES);
    assert_eq!(transcript.latest_answer(), Some("latest answer"));
    assert!(transcript.omitted_activity > 0);
}

#[test]
fn subagent_display_marks_overflow_after_an_exactly_full_chunk_once() {
    use crate::tui::state::subagent_history::{append_display_text, DISPLAY_TEXT_BYTES};
    let mut text = String::new();
    append_display_text(&mut text, &"x".repeat(DISPLAY_TEXT_BYTES));
    assert_eq!(text.len(), DISPLAY_TEXT_BYTES);
    append_display_text(&mut text, "");
    assert_eq!(text.len(), DISPLAY_TEXT_BYTES);
    append_display_text(&mut text, "more");
    assert!(text.ends_with("[history truncated]"));
    let marked = text.clone();
    append_display_text(&mut text, "later");
    assert_eq!(text, marked);
}

#[test]
fn workflow_scale_tree_retains_live_runs_and_evicts_whole_finished_runs() {
    let (worker, _rx) = mpsc::unbounded_channel();
    let mut app = test_app();
    start_named_test_subagent(&mut app, &worker, 1, None, 0, "workflow", "workflow review");
    for id in 2..=55 {
        let parent = if id < 15 { 1 } else { 2 + (id % 12) };
        start_named_test_subagent(
            &mut app,
            &worker,
            id,
            Some(parent),
            if id < 15 { 1 } else { 2 },
            "workflow",
            &format!("stage {id}"),
        );
        if id < 15 {
            finish_test_subagent(
                &mut app,
                &worker,
                id,
                "workflow",
                HarnessEvent::Result {
                    message: "done".into(),
                },
            );
        }
    }
    for id in 100..220 {
        start_named_test_subagent(&mut app, &worker, id, None, 0, "other", "other");
        finish_test_subagent(
            &mut app,
            &worker,
            id,
            "other",
            HarnessEvent::Result {
                message: "done".into(),
            },
        );
    }
    assert!((1..=55).all(|id| app.subagent_transcripts.contains_key(&id)));
    handle_ui_msg(
        &mut app,
        UiMsg::Event(HarnessEvent::ToolResult {
            tool_call_id: "workflow".into(),
            tool_name: "workflow".into(),
            output: serde_json::json!({"runId":1,"status":"running","termination":"detached"}),
            is_error: false,
        }),
        &worker,
        100,
    );
    assert_eq!(
        app.subagent_transcripts[&1].status,
        crate::tui::state::SubagentTranscriptStatus::Running
    );
    let rows = agent_tree_rows(&app);
    assert!(rows.iter().any(|row| row.prefix.chars().count() > 3));
    app.agent_browser = Some(crate::tui::state::AgentBrowser::new(rows.len()));
    let rendered = rendered_rows(&mut app, 100, 24).join("\n");
    assert!(rendered.contains("#24"), "{rendered}");
    std::fs::write(std::env::temp_dir().join("orca-dag-browser.txt"), rendered).unwrap();
    for id in 1..=55 {
        finish_test_subagent(
            &mut app,
            &worker,
            id,
            "workflow",
            HarnessEvent::Result {
                message: "done".into(),
            },
        );
    }
    for id in 300..410 {
        start_named_test_subagent(&mut app, &worker, id, None, 0, "new", "new");
        finish_test_subagent(
            &mut app,
            &worker,
            id,
            "new",
            HarnessEvent::Result {
                message: "done".into(),
            },
        );
    }
    assert!((1..=55).all(|id| !app.subagent_transcripts.contains_key(&id)));
}

#[test]
fn workflow_call_stays_compact_regardless_of_graph_size() {
    let graph: Vec<_> = (0..50)
        .map(|id| serde_json::json!({"id":format!("s{id}"),"prompt":"private stage prompt"}))
        .collect();
    assert_eq!(
        crate::presentation::tool_call_line(
            "workflow",
            &serde_json::json!({"action":"run","graph":graph})
        ),
        "workflow 50 stages"
    );
    assert_eq!(
        crate::presentation::tool_action_label("workflow"),
        "Workflow"
    );
}
