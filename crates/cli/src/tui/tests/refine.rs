#[cfg(test)]
mod refine_command_tests {
    use super::*;
    use crate::msg::WorkerCmd;
    use crate::refine::{RefineOutcome, ScriptFile, SkillProposal};

    fn test_app_with_skills(tag: &str) -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "orca-tui-refine-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let app = App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
            workspace_root: "/test-ws".into(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: crate::skills::Skills::new(&dir, None, None),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        });
        (app, dir)
    }

    fn printed(app: &App) -> String {
        app.pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn proposal(scripts: Vec<ScriptFile>) -> SkillProposal {
        SkillProposal {
            name: "retry-with-backoff".into(),
            description: "Use jittered exponential backoff.".into(),
            body: "Cap the delay.".into(),
            scripts,
            citations: vec!["e03".into()],
        }
    }

    #[test]
    fn refine_and_undo_send_their_worker_commands() {
        let (mut app, dir) = test_app_with_skills("send");
        let (tx, mut rx) = mpsc::unbounded_channel();

        slash_command(&mut app, "refine", &tx, 90);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::Refine)));
        assert!(printed(&app).contains("reviewing the trajectory"));

        slash_command(&mut app, "refine undo", &tx, 90);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::RefineUndo)));

        slash_command(&mut app, "refine bogus", &tx, 90);
        assert!(rx.try_recv().is_err());
        assert!(printed(&app).contains("unknown: /refine bogus"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refine_done_renders_the_house_style_block() {
        let (mut app, dir) = test_app_with_skills("done");
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = RefineOutcome {
            proposal: proposal(vec![ScriptFile {
                path: "scripts/check_backoff.py".into(),
                code: "print(1)".into(),
            }]),
            checks: vec![crate::refine::Check {
                ok: true,
                msg: "all citations resolve in the roster".into(),
            }],
            repaired: true,
        };
        handle_ui_msg(&mut app, UiMsg::RefineDone(Box::new(Ok(outcome))), &tx, 90);

        let shown = printed(&app);
        assert!(shown.contains("refine"));
        assert!(shown.contains("proposed skill retry-with-backoff"));
        assert!(shown.contains("packages scripts/check_backoff.py"));
        assert!(shown.contains("evidence: e03"));
        // Checks collapse to one summary row: the itemized list only
        // mattered on failure, and failures arrive as errors instead.
        assert!(shown.contains("✓ 1 check passed · after one repair retry"));
        assert!(!shown.contains("all citations resolve"));
        // Tree branches, not raw dumped text.
        assert!(shown.contains("├") && shown.contains("└"));
        // Accept/reject is the approval gate's job now, not a typed command.
        assert!(!shown.contains("/refine apply"));
        assert_eq!(app.turn_count, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_refine_approval_is_yes_no_only() {
        let (mut app, dir) = test_app_with_skills("yesno");

        let (respond, mut answer) = tokio::sync::oneshot::channel();
        app.approval = Some(crate::msg::ApprovalRequest {
            tool_name: "refine".into(),
            detail: "apply skill retry-with-backoff".into(),
            yes_no: true,
            respond,
        });

        // The pinned prompt offers only yes/no.
        let prompt = live_lines(&app, 120)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(prompt.contains("[y] yes"));
        assert!(prompt.contains("[n] no"));
        assert!(!prompt.contains("always"));

        // a and A are dead keys here; y answers.
        for key in ['a', 'A'] {
            handle_approval_key(&mut app, KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
            assert!(app.approval.is_some(), "{key} must not answer a yes/no prompt");
            assert!(answer.try_recv().is_err());
        }
        handle_approval_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        );
        assert_eq!(answer.try_recv().unwrap(), crate::msg::ApprovalResponse::AllowOnce);
        assert!(app.approval.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scriptless_proposal_says_so_and_errors_use_the_notification_glyph() {
        let (mut app, dir) = test_app_with_skills("empty");
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = RefineOutcome {
            proposal: proposal(Vec::new()),
            checks: Vec::new(),
            repaired: false,
        };
        handle_ui_msg(&mut app, UiMsg::RefineDone(Box::new(Ok(outcome))), &tx, 90);
        assert!(printed(&app).contains("packages no scripts"));

        handle_ui_msg(
            &mut app,
            UiMsg::RefineDone(Box::new(Err("nothing to refine".into()))),
            &tx,
            90,
        );
        assert!(printed(&app).contains("• refine: nothing to refine"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
