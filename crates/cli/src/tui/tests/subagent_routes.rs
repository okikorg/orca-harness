#[cfg(test)]
mod subagent_route_tests {
    use super::*;
    use orca_harness_tools::SubagentDepth;

    #[test]
    fn route_picker_explains_every_policy_in_the_rendered_buffer() {
        let mut app = super::subagents_command_tests::depth_app(SubagentDepth::new(1));
        app.overlay = Some(Overlay::SubagentValues {
            setting: crate::tui::state::SubagentSetting::Route,
            values: [
                "inherit",
                "auto",
                "preference",
                "local",
                "flash",
                "mid",
                "frontier",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            picker: ListPicker::new(7),
        });

        let backend = ratatui::backend::TestBackend::new(100, 11);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let lines = live_lines(&app, 100);
        terminal
            .draw(|frame| {
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(ratatui::text::Text::from(lines)),
                    frame.area(),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0..11)
            .map(|y| {
                (0..100)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        for (route, explanation) in [
            ("inherit", "always use the parent model"),
            ("auto", "model chooses per subagent"),
            (
                "preference",
                "model chooses only from your preferred models",
            ),
            ("local", "always use the preferred local model"),
            ("flash", "always use the preferred fast model"),
            ("mid", "always use the preferred balanced model"),
            ("frontier", "always use the preferred strongest model"),
        ] {
            assert!(
                rendered
                    .iter()
                    .any(|line| line.contains(route) && line.contains(explanation)),
                "missing `{route}` explanation in:\n{}",
                rendered.join("\n")
            );
        }
    }

    #[test]
    fn model_chosen_routes_are_selectable() {
        let settings = SubagentDepth::new(1);
        assert_eq!(
            crate::tui::subagents::subagent_values(
                &settings,
                crate::tui::state::SubagentSetting::Route,
            ),
            vec![
                "inherit".to_string(),
                "auto".to_string(),
                "preference".to_string(),
            ]
        );
        for route in ["auto", "preference"] {
            crate::tui::subagents::apply_subagent_value(
                &settings,
                crate::tui::state::SubagentSetting::Route,
                route,
            );
            assert_eq!(settings.model_route().as_deref(), Some(route));
        }
    }
    #[test]
    fn subagent_numeric_controls_render_with_last_row_visible() {
        use crate::tui::state::{SubagentSetting, SUBAGENT_ROWS};
        let mut app = super::subagents_command_tests::depth_app(SubagentDepth::default());
        app.overlay = Some(Overlay::Subagents {
            picker: ListPicker::with_selected(SUBAGENT_ROWS, SUBAGENT_ROWS - 1),
        });
        let render = |app: &App| {
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        ratatui::widgets::Paragraph::new(ratatui::text::Text::from(live_lines(
                            app, 80,
                        ))),
                        frame.area(),
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..24)
                .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let list = render(&app);
        assert!(list.contains("max retry delay"), "{list}");
        app.overlay = Some(Overlay::SubagentNumber {
            setting: SubagentSetting::ModelConcurrency,
            input: "71".into(),
            error: String::new(),
        });
        let editor = render(&app);
        assert!(
            editor.contains("provider streams")
                && editor.contains("> 71")
                && editor.contains("shared with parent"),
            "{editor}"
        );
        let setting = SubagentSetting::Steps;
        let values = crate::tui::subagents::subagent_values(&app.cfg.subagent_depth, setting);
        app.overlay = Some(Overlay::SubagentValues {
            setting,
            picker: ListPicker::new(values.len()),
            values,
        });
        let presets = render(&app);
        assert!(presets.lines().any(|line| line.trim() == "96"), "{presets}");
        assert!(presets.contains("custom…"), "{presets}");
        println!("{list}\n{presets}\n{editor}");
    }
}
