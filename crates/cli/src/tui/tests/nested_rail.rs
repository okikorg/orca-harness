mod nested_rail_tests {
    use super::*;
    use crate::tui::events::start_subagent;
    use orca_harness_extensions::HarnessEvent;
    use serde_json::json;

    fn nested_app() -> App {
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

    fn rail_text(app: &App) -> String {
        activity_lines(app, 120, true)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn subagent_row_names_resolved_provider_and_model() {
        let mut app = nested_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "Explore the benchmarks directory"}),
            },
            120,
        );
        start_subagent(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            "Explore the benchmarks directory".into(),
            Some(orca_harness_tools::SubagentIdentity::new(
                "openrouter",
                "anthropic/claude-sonnet-5",
            )),
        );

        let live = rail_text(&app);
        assert!(
            live.contains("Subagent · openrouter:anthropic/claude-sonnet-5 · Explore the benchmarks directory"),
            "{live}"
        );
        assert!(!live.contains("{\"task\""), "raw JSON should be replaced: {live}");

        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                output: json!({
                    "answer": "done",
                    "identity": {
                        "provider": "openrouter",
                        "model": "anthropic/claude-sonnet-5",
                        "route": "frontier/claude-sonnet-5"
                    }
                }),
                is_error: false,
            },
            120,
        );
        let completed = activity_lines(&app, 120, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            completed.contains("Subagent · openrouter:anthropic/claude-sonnet-5 · Explore the benchmarks directory"),
            "{completed}"
        );

        if let Some(tool) = app.activity_tools.first_mut() {
            tool.output = Some(json!({"error": "worker timed out"}));
            tool.is_error = true;
        }
        let failed = activity_lines(&app, 120, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            failed.contains("Subagent · openrouter:anthropic/claude-sonnet-5 · Explore the benchmarks directory"),
            "{failed}"
        );
    }

    #[test]
    fn inner_tools_render_indented_under_the_subagent_line() {
        let mut app = nested_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "explore"}),
            },
            120,
        );
        handle_subagent_event(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                input: json!({"path": "."}),
            },
        );
        let text = rail_text(&app);
        assert!(text.contains("Subagent"), "rail: {text}");
        assert!(text.contains("List directory · "), "rail: {text}");
        let inner_line = text.lines().find(|l| l.contains("List directory · ")).unwrap();
        assert!(
            inner_line.starts_with("      "),
            "inner line must be indented: {inner_line:?}"
        );
        assert!(
            inner_line.contains("└─") || inner_line.contains("├─"),
            "inner line must carry a tree branch so ownership is unambiguous: {inner_line:?}"
        );
        let outer_line = text.lines().find(|l| l.contains("Subagent")).unwrap();
        let branch_col = |l: &str| l.find(['└', '├']).unwrap();
        assert!(
            branch_col(inner_line) > branch_col(outer_line),
            "inner branch must sit deeper than the subagent's own branch:\n{outer_line}\n{inner_line}"
        );

        handle_subagent_event(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolResult {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                output: json!({"entries": []}),
                is_error: false,
            },
        );
        let text = rail_text(&app);
        let inner_line = text.lines().find(|l| l.contains("List directory · ")).unwrap();
        assert!(inner_line.contains("✓"), "completed glyph: {inner_line:?}");
    }

    #[test]
    fn deeper_spawns_indent_further() {
        let mut app = nested_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "outer"}),
            },
            120,
        );
        handle_subagent_event(
            &mut app,
            1,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "i1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "inner"}),
            },
        );
        handle_subagent_event(
            &mut app,
            2,
            Some(1),
            1,
            "i1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "g1".into(),
                tool_name: "grep".into(),
                input: json!({"pattern": "x"}),
            },
        );
        let text = rail_text(&app);
        let child = text.lines().find(|l| l.contains("Subagent · inner")).unwrap();
        let grandchild = text.lines().find(|l| l.contains("Search")).unwrap();
        let indent = |l: &str| l.chars().take_while(|c| *c == ' ').count();
        assert!(
            indent(grandchild) > indent(child),
            "child: {child:?} grandchild: {grandchild:?}"
        );
    }

    #[test]
    fn parallel_nested_subagents_keep_identity_and_children_under_their_own_rows() {
        let mut app = nested_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "root".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "root"}),
            },
            140,
        );
        start_subagent(
            &mut app,
            1,
            None,
            0,
            "root".into(),
            "root".into(),
            Some(orca_harness_tools::SubagentIdentity::new("openrouter", "root/model")),
        );
        for (call_id, task, model, inner_tool) in [
            ("root", "task alpha", "vendor/alpha", "read_file"),
            ("b", "task beta", "vendor/beta", "grep"),
        ] {
            handle_subagent_event(
                &mut app,
                1,
                None,
                0,
                "root".into(),
                HarnessEvent::ToolCall {
                    tool_call_id: call_id.into(),
                    tool_name: "subagent".into(),
                    input: json!({"task": task}),
                },
            );
            let child_id = if call_id == "root" { 2 } else { 3 };
            start_subagent(
                &mut app,
                child_id,
                Some(1),
                1,
                call_id.into(),
                task.into(),
                Some(orca_harness_tools::SubagentIdentity::new("openrouter", model)),
            );
            handle_subagent_event(
                &mut app,
                child_id,
                Some(1),
                1,
                call_id.into(),
                HarnessEvent::ToolCall {
                    tool_call_id: format!("{call_id}-inner"),
                    tool_name: inner_tool.into(),
                    input: json!({}),
                },
            );
        }
        handle_subagent_event(
            &mut app,
            1,
            None,
            0,
            "root".into(),
            HarnessEvent::ToolResult {
                tool_call_id: "root".into(),
                tool_name: "subagent".into(),
                output: json!({"answer": "alpha done"}),
                is_error: false,
            },
        );

        let text = rail_text(&app);
        assert_eq!(text.matches("openrouter:vendor/alpha").count(), 1, "{text}");
        assert_eq!(text.matches("openrouter:vendor/beta").count(), 1, "{text}");
        assert!(text.contains("✓ Subagent · openrouter:vendor/alpha · task alpha"), "{text}");
        let alpha = text.find("vendor/alpha").unwrap();
        let beta = text.find("vendor/beta").unwrap();
        let grep = text.find("Search").unwrap();
        assert!(alpha < beta && beta < grep, "beta's child stays below beta: {text}");
        assert!(!text[alpha..beta].contains("Search"), "grep must not appear under alpha: {text}");

        start_subagent(
            &mut app,
            4,
            Some(1),
            1,
            "b".into(),
            "task beta retry".into(),
            Some(orca_harness_tools::SubagentIdentity::new(
                "openrouter",
                "vendor/beta-retry",
            )),
        );
        let retried = rail_text(&app);
        assert!(!retried.contains("openrouter:vendor/beta ·"), "stale attempt: {retried}");
        assert_eq!(retried.matches("openrouter:vendor/beta-retry").count(), 1, "{retried}");
    }

    #[test]
    fn completion_folds_inner_log_into_the_expandable_record() {
        let mut app = nested_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "explore"}),
            },
            120,
        );
        handle_subagent_event(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                input: json!({"path": "."}),
            },
        );
        handle_subagent_event(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolResult {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                output: json!({"entries": []}),
                is_error: false,
            },
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                output: json!({"answer": "found things"}),
                is_error: false,
            },
            120,
        );

        assert!(
            app.subagent_activity.is_empty(),
            "spawn state must fold away"
        );
        let record = app.tool_log.last().unwrap();
        assert!(record.inner.iter().any(|l| l.contains("list_dir")));

        expand_tool(&mut app, 1, 120);
        let expanded: String = app
            .pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded.contains("inner activity"), "{expanded}");
        assert!(expanded.contains("list_dir"), "{expanded}");
    }
}
