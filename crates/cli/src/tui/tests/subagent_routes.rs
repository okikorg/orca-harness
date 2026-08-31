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
            ("flash", "always use the preferred fast cloud model"),
            ("mid", "always use the preferred balanced cloud model"),
            ("frontier", "always use the preferred strongest cloud model"),
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
}
