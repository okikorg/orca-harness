#[cfg(test)]
mod theme_command_tests {
    use super::*;

    fn theme_app() -> App {
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

    /// One test, single-threaded, because the theme is process-global: it
    /// must not interleave with any other mutating test. Ends by restoring
    /// the default so later tests see a clean state.
    #[test]
    fn theme_command_switches_and_rejects() {
        let _theme = THEME_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let mut app = theme_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        for (cmd, expected) in [
            ("theme dracula", view::ThemeName::Dracula),
            ("theme solarized-dark", view::ThemeName::SolarizedDark),
            ("theme one-dark", view::ThemeName::OneDark),
            ("theme monokai", view::ThemeName::Monokai),
            ("theme nord", view::ThemeName::Nord),
            ("theme default", view::ThemeName::Default),
            ("theme mono", view::ThemeName::Mono),
        ] {
            slash_command(&mut app, cmd, &worker, 80);
            assert_eq!(view::theme_name(), expected, "command {cmd}");
        }

        // Unknown names are rejected and leave the theme unchanged.
        slash_command(&mut app, "theme midnight", &worker, 80);
        assert_eq!(
            view::theme_name(),
            view::ThemeName::Mono,
            "unchanged on garbage"
        );

        // Bare form only reports; it must not change the value.
        slash_command(&mut app, "theme", &worker, 80);
        assert_eq!(
            view::theme_name(),
            view::ThemeName::Mono,
            "bare form does not change"
        );
        let text = app
            .pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Mono"), "reports current name: {text}");

        // Restore the default so other tests (and the view) see clean state.
        slash_command(&mut app, "theme color", &worker, 80); // legacy alias
        assert_eq!(
            view::theme_name(),
            view::ThemeName::Default,
            "color stays a legacy alias for default"
        );
    }
}

#[cfg(test)]
mod subagents_command_tests {
    use super::*;
    use orca_harness_tools::SubagentDepth;

    fn depth_app(depth: SubagentDepth) -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: depth,
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: Default::default(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        })
    }

    #[tokio::test]
    async fn subagents_command_sets_and_clamps_depth() {
        let depth = SubagentDepth::new(1);
        let mut app = depth_app(depth.clone());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "subagents 3", &worker, 80);
        assert_eq!(depth.get(), 3);

        slash_command(&mut app, "subagents 99", &worker, 80);
        assert_eq!(depth.get(), 5, "out-of-range input clamps");

        // Bare form only reports; it must not change the value.
        slash_command(&mut app, "subagents", &worker, 80);
        assert_eq!(depth.get(), 5);

        // Garbage input leaves the value alone.
        slash_command(&mut app, "subagents lots", &worker, 80);
        assert_eq!(depth.get(), 5);
    }
}

#[cfg(test)]
mod mode_rewind_todo_tests {
    use super::*;
    use crate::mode::{Mode, ModeHandle};
    use orca_harness_tools::TodoList;

    fn app_with(mode: ModeHandle, todos: TodoList) -> App {
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
            mode,
            todos,
            plan: Default::default(),
        })
    }

    fn texts(app: &App) -> String {
        app.pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn mode_toggles_bare_and_sets_by_name() {
        let mode = ModeHandle::default();
        let mut app = app_with(mode.clone(), TodoList::new());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "mode", &worker, 80);
        assert_eq!(mode.get(), Mode::Plan, "bare /mode toggles");
        slash_command(&mut app, "mode", &worker, 80);
        assert_eq!(mode.get(), Mode::Normal);

        slash_command(&mut app, "mode plan", &worker, 80);
        assert_eq!(mode.get(), Mode::Plan);
        // Setting the mode it is already in is not a toggle.
        slash_command(&mut app, "mode plan", &worker, 80);
        assert_eq!(mode.get(), Mode::Plan);
        slash_command(&mut app, "mode normal", &worker, 80);
        assert_eq!(mode.get(), Mode::Normal);

        // Garbage leaves the mode alone and says so.
        slash_command(&mut app, "mode sideways", &worker, 80);
        assert_eq!(mode.get(), Mode::Normal);
        // Glyphed like every other system line, not flush-left.
        assert!(texts(&app).contains("• unknown mode: sideways"));
    }

    /// Leaving plan mode reports the plans that were actually written,
    /// and stays quiet when the agent decided none was warranted —
    /// looking around in plan mode is a legitimate use of it.
    #[tokio::test]
    async fn leaving_plan_mode_reports_only_plans_that_were_written() {
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        // An episode where the agent judged no plan was needed: silence.
        let mut app = app_with(ModeHandle::new(Mode::Plan), TodoList::new());
        slash_command(&mut app, "mode normal", &worker, 80);
        let rendered = texts(&app);
        assert!(rendered.contains("normal mode"), "{rendered}");
        assert!(!rendered.contains("plan saved"), "{rendered}");
        assert!(!rendered.contains("no plan"), "{rendered}");

        // An episode where it wrote two.
        let mut app = app_with(ModeHandle::new(Mode::Plan), TodoList::new());
        app.cfg.plan.record("docs/plan/2026-08-22-first.md");
        app.cfg.plan.record("docs/plan/2026-08-22-second.md");
        slash_command(&mut app, "mode normal", &worker, 80);
        let rendered = texts(&app);
        assert!(
            rendered.contains("plan saved to docs/plan/2026-08-22-first.md"),
            "{rendered}"
        );
        assert!(
            rendered.contains("plan saved to docs/plan/2026-08-22-second.md"),
            "{rendered}"
        );
        assert!(app.cfg.plan.written().is_empty(), "the episode ended");
    }

    /// Entering plan mode must not claim anything about files — at that
    /// point nobody knows whether the conversation warrants one.
    #[tokio::test]
    async fn entering_plan_mode_says_nothing_about_files() {
        let mut app = app_with(ModeHandle::new(Mode::Normal), TodoList::new());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        slash_command(&mut app, "mode plan", &worker, 80);
        let rendered = texts(&app);
        assert!(rendered.contains("plan mode"), "{rendered}");
        assert!(!rendered.contains("plan saved"), "{rendered}");
        assert!(!rendered.contains("docs/plan"), "{rendered}");
    }

    /// Plan mode is a restriction the user must not be able to lose
    /// track of, so it is on the status line while it is on and absent
    /// when it is not.
    #[test]
    fn plan_mode_shows_in_the_status_line() {
        let mode = ModeHandle::default();
        let plan = crate::plan::PlanArea::new();
        assert_eq!(mode_segment(&mode, &plan), "");
        mode.set(Mode::Plan);
        assert_eq!(mode_segment(&mode, &plan), " · plan mode");
        // A landed plan is visible without waiting for /mode normal.
        plan.record("docs/plan/2026-08-22-a.md");
        assert_eq!(mode_segment(&mode, &plan), " · plan mode · 1 plan");
        plan.record("docs/plan/2026-08-22-b.md");
        assert_eq!(mode_segment(&mode, &plan), " · plan mode · 2 plans");
        // Normal mode says nothing, whatever was written.
        mode.set(Mode::Normal);
        assert_eq!(mode_segment(&mode, &plan), "");
    }

    /// Write a task list through the real tool, the way the model does.
    async fn set_todos(todos: &TodoList, items: serde_json::Value) {
        let tool = orca_harness_tools::TodoWriteTool::new(todos.clone());
        let ctx = orca_harness_core::ToolContext {
            call_id: "c".into(),
            tool_name: "todo_write".into(),
            cancellation: orca_harness_core::CancellationToken::new(),
            deadline: None,
        };
        orca_harness_core::Tool::call(&tool, serde_json::json!({ "todos": items }), &ctx)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn todo_progress_shows_in_the_status_line_once_there_is_a_list() {
        let todos = TodoList::new();
        assert_eq!(todo_segment(&todos), "", "silent with no list");
        set_todos(
            &todos,
            serde_json::json!([
                {"content": "a", "status": "completed"},
                {"content": "b", "status": "in_progress"},
                {"content": "c"}
            ]),
        )
        .await;
        assert_eq!(todo_segment(&todos), " · todo 1/3");
    }

    #[tokio::test]
    async fn todo_progress_pins_the_full_plan_in_the_live_region() {
        let todos = TodoList::new();
        set_todos(
            &todos,
            serde_json::json!([
                {"content": "inspect the rendering", "status": "completed"},
                {"content": "add a visible progress cue", "status": "in_progress"},
                {"content": "verify it"}
            ]),
        )
        .await;
        let app = app_with(ModeHandle::default(), todos);

        let rendered = live_lines(&app, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("todo · 1/3 done"), "{rendered}");
        assert!(rendered.contains("├ ✓ inspect the rendering"), "{rendered}");
        assert!(
            rendered.contains("├ ▸ add a visible progress cue"),
            "{rendered}"
        );
        assert!(rendered.contains("└ □ verify it"), "{rendered}");
    }

    #[tokio::test]
    async fn completed_todo_progress_says_complete() {
        let todos = TodoList::new();
        set_todos(
            &todos,
            serde_json::json!([
                {"content": "inspect", "status": "completed"},
                {"content": "verify", "status": "completed"}
            ]),
        )
        .await;
        let app = app_with(ModeHandle::default(), todos);

        let rendered = live_lines(&app, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("todo · 2/2 done"), "{rendered}");
        assert!(rendered.contains("├ ✓ inspect"), "{rendered}");
        assert!(rendered.contains("└ ✓ verify"), "{rendered}");
    }

    #[tokio::test]
    async fn todo_renders_the_list_and_says_so_when_there_is_none() {
        let todos = TodoList::new();
        let mut app = app_with(ModeHandle::default(), todos.clone());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "todo", &worker, 80);
        assert!(texts(&app).contains("no task list"));

        set_todos(
            &todos,
            serde_json::json!([
                {"content": "read the code", "status": "completed"},
                {"content": "write the fix", "status": "in_progress"}
            ]),
        )
        .await;

        slash_command(&mut app, "todo", &worker, 80);
        let rendered = texts(&app);
        assert!(rendered.contains("1/2 done"), "{rendered}");
        assert!(rendered.contains("✓ read the code"), "{rendered}");
        assert!(rendered.contains("▸ write the fix"), "{rendered}");
    }

    #[tokio::test]
    async fn rewind_sends_the_turn_count_and_rejects_nonsense() {
        let mut app = app_with(ModeHandle::default(), TodoList::new());
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "rewind", &worker, 80);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::Rewind { turns: 1 })));

        slash_command(&mut app, "rewind 3", &worker, 80);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::Rewind { turns: 3 })));

        // Zero and garbage send nothing and explain themselves.
        slash_command(&mut app, "rewind 0", &worker, 80);
        slash_command(&mut app, "rewind lots", &worker, 80);
        assert!(rx.try_recv().is_err(), "bad input sends no command");
        assert!(texts(&app).contains("usage: /rewind"));
    }

    #[tokio::test]
    async fn fork_asks_the_worker_to_branch() {
        let mut app = app_with(ModeHandle::default(), TodoList::new());
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();
        slash_command(&mut app, "fork", &worker, 80);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::Fork)));
    }

    /// A rewind redraws the transcript from the shortened context, but
    /// the tokens it already spent are not conversation state.
    #[test]
    fn rewind_redraws_the_transcript_and_keeps_the_token_totals() {
        let mut app = app_with(ModeHandle::default(), TodoList::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        app.tokens_in = 1200;
        app.tokens_out = 340;
        app.usage_steps = 4;
        app.context_tokens = 9000;

        handle_ui_msg(
            &mut app,
            UiMsg::ContextRewound {
                messages: vec![
                    orca_harness_core::Message::System {
                        content: "sys".into(),
                    },
                    orca_harness_core::Message::User {
                        content: "still here".into(),
                        images: Vec::new(),
                    },
                ],
                notice: "rewound 1 turn · 2 messages dropped".into(),
            },
            &tx,
            80,
        );

        let rendered = texts(&app);
        assert!(rendered.contains("rewound 1 turn"), "{rendered}");
        assert!(rendered.contains("still here"), "{rendered}");
        assert_eq!(app.tokens_in, 1200, "spent tokens are not un-spent");
        assert_eq!(app.tokens_out, 340);
        assert_eq!(app.usage_steps, 4);
        assert_eq!(app.turn_count, 1, "turn count follows the new transcript");
        assert_eq!(app.context_tokens, 0, "occupancy waits for the next step");
    }

    #[test]
    fn forking_moves_the_session_id_without_touching_the_transcript() {
        let mut app = app_with(ModeHandle::default(), TodoList::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        app.cfg.session_id = Some("old-id".into());
        app.turn_count = 3;

        handle_ui_msg(
            &mut app,
            UiMsg::SessionForked {
                id: "new-id".into(),
                parent: "old-id".into(),
            },
            &tx,
            80,
        );

        assert_eq!(app.cfg.session_id.as_deref(), Some("new-id"));
        assert_eq!(app.turn_count, 3, "the conversation did not change");
        let rendered = texts(&app);
        assert!(rendered.contains("forked to session new-id"), "{rendered}");
        assert!(rendered.contains("old-id is left as it was"), "{rendered}");
    }
}

