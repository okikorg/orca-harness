#[cfg(test)]
mod subagents_command_tests {
    use super::*;
    use crate::tui::state::{SubagentSetting, SUBAGENT_ROWS};
    use orca_harness_tools::SubagentDepth;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub(super) fn depth_app(depth: SubagentDepth) -> App {
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
    async fn subagents_picker_accepts_custom_numbers_and_unlimited() {
        let settings = SubagentDepth::new(1);
        let mut app = depth_app(settings.clone());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        for (setting, input, expected) in [
            (SubagentSetting::Background, "4097", "4097"),
            (SubagentSetting::Background, "unlimited", "unlimited"),
            (SubagentSetting::Steps, "137", "137"),
            (SubagentSetting::ModelConcurrency, "71", "71"),
            (SubagentSetting::ModelAttempts, "0", "unlimited"),
            (SubagentSetting::ParallelTools, "37", "37"),
        ] {
            app.overlay = Some(Overlay::Subagents {
                picker: ListPicker::with_selected(
                    SUBAGENT_ROWS,
                    SubagentSetting::ALL
                        .iter()
                        .position(|s| *s == setting)
                        .unwrap(),
                ),
            });
            handle_overlay_key(&mut app, key(KeyCode::Enter), &worker);
            assert!(matches!(app.overlay, Some(Overlay::SubagentValues { .. })));
            let Some(Overlay::SubagentValues {
                ref values,
                ref picker,
                ..
            }) = app.overlay
            else {
                unreachable!()
            };
            for _ in picker.index()..values.len() - 1 {
                handle_overlay_key(&mut app, key(KeyCode::Down), &worker);
            }
            handle_overlay_key(&mut app, key(KeyCode::Enter), &worker);
            assert!(matches!(app.overlay, Some(Overlay::SubagentNumber { .. })));
            let (first, rest) = input.split_at(1);
            handle_overlay_key(
                &mut app,
                key(KeyCode::Char(first.chars().next().unwrap())),
                &worker,
            );
            handle_terminal_event(&mut app, CtEvent::Paste(rest.into()), &worker, 80);
            handle_overlay_key(&mut app, key(KeyCode::Enter), &worker);
            assert_eq!(
                crate::tui::subagents::subagent_current(&settings, setting),
                expected
            );
            assert!(matches!(app.overlay, Some(Overlay::Subagents { .. })));
            assert!(app.overlay_stack.is_empty());
        }
        app.overlay = Some(Overlay::SubagentNumber {
            setting: SubagentSetting::Steps,
            input: "0".into(),
            error: String::new(),
        });
        handle_overlay_key(&mut app, key(KeyCode::Enter), &worker);
        assert_eq!(settings.max_steps(), 137);
        assert!(
            matches!(app.overlay, Some(Overlay::SubagentNumber { ref error, .. }) if !error.is_empty())
        );
        handle_overlay_key(&mut app, key(KeyCode::Esc), &worker);
        assert!(app.overlay.is_none());
    }

    #[tokio::test]
    async fn subagents_presets_keep_lower_and_upper_choices_selectable() {
        let settings = SubagentDepth::default();
        let mut app = depth_app(settings.clone());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        for (setting, lower, upper) in [
            (SubagentSetting::Depth, "1", "5"),
            (SubagentSetting::Background, "1", "64"),
            (SubagentSetting::Steps, "6", "96"),
            (SubagentSetting::Timeout, "30", "1800"),
            (SubagentSetting::Output, "1000", "64000"),
            (SubagentSetting::ToolAttempts, "1", "10"),
            (SubagentSetting::Backoff, "0", "2000"),
        ] {
            for value in [lower, upper] {
                app.overlay = Some(Overlay::Subagents {
                    picker: ListPicker::with_selected(
                        SUBAGENT_ROWS,
                        SubagentSetting::ALL
                            .iter()
                            .position(|s| *s == setting)
                            .unwrap(),
                    ),
                });
                handle_overlay_key(&mut app, key(KeyCode::Enter), &worker);
                let Some(Overlay::SubagentValues { values, picker, .. }) = app.overlay.as_mut()
                else {
                    panic!("preset picker");
                };
                *picker = ListPicker::with_selected(
                    values.len(),
                    values
                        .iter()
                        .position(|v| v == value)
                        .expect("original choice retained"),
                );
                handle_overlay_key(&mut app, key(KeyCode::Enter), &worker);
                assert_eq!(setting.numeric().unwrap().current(&settings), value);
                assert!(matches!(app.overlay, Some(Overlay::Subagents { .. })));
            }
        }
        settings.set_max_steps(137);
        let values = crate::tui::subagents::subagent_values(&settings, SubagentSetting::Steps);
        let selected =
            crate::tui::subagents::subagent_selected(&settings, SubagentSetting::Steps, &values);
        assert_eq!(values[selected], "137", "saved custom values stay selected");
    }

    #[tokio::test]
    async fn subagents_command_sets_custom_depth() {
        let depth = SubagentDepth::new(1);
        let mut app = depth_app(depth.clone());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "subagents 3", &worker, 80);
        assert_eq!(depth.get(), 3);

        slash_command(&mut app, "subagents 99", &worker, 80);
        assert_eq!(depth.get(), 99, "custom depth has no preset ceiling");

        // Bare form opens the full live settings menu and changes nothing.
        slash_command(&mut app, "subagents", &worker, 80);
        assert_eq!(depth.get(), 99);
        assert!(matches!(app.overlay, Some(Overlay::Subagents { .. })));
        handle_overlay_key(&mut app, key(KeyCode::Esc), &worker);

        // Garbage input leaves the value alone.
        slash_command(&mut app, "subagents lots", &worker, 80);
        assert_eq!(depth.get(), 99);
    }
}
