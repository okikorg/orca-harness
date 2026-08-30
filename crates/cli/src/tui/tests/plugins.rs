#[cfg(test)]
mod plugin_command_tests {
    use super::*;

    fn plugin_app(workspace: &std::path::Path) -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
            workspace_root: workspace.display().to_string(),
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

    fn overlay_text(app: &App) -> String {
        live_lines(app, 140)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn printed(app: &App) -> String {
        app.pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(app: &mut App, worker: &mpsc::UnboundedSender<WorkerCmd>, code: KeyCode) {
        handle_terminal_event(
            app,
            CtEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            worker,
            140,
        );
    }

    #[test]
    fn picker_lists_configured_and_live_state_and_filters() {
        let workspace = std::path::PathBuf::from("/test-ws");
        let mut app = plugin_app(&workspace);
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.overlay = Some(Overlay::Plugins {
            entries: vec![
                crate::config::RegisteredPlugin {
                    name: "alpha-plugin".into(),
                    root: workspace.join("alpha"),
                    enabled: true,
                },
                crate::config::RegisteredPlugin {
                    name: "beta-plugin".into(),
                    root: workspace.join("beta"),
                    enabled: false,
                },
            ],
            filter: String::new(),
            picker: ListPicker::new(2).actions(crate::tui::commands::PLUGIN_ACTIONS),
        });

        let text = overlay_text(&app);
        assert!(text.contains("Plugins"), "{text}");
        assert!(text.contains("alpha-plugin"), "{text}");
        assert!(text.contains("restart to load"), "{text}");
        assert!(text.contains("beta-plugin"), "{text}");
        assert!(text.contains("not loaded"), "{text}");
        assert!(text.contains("space actions"), "{text}");

        press(&mut app, &worker, KeyCode::Char('b'));
        let filtered = overlay_text(&app);
        assert!(filtered.contains("filter: b"), "{filtered}");
        assert!(!filtered.contains("alpha-plugin"), "{filtered}");
        assert!(filtered.contains("beta-plugin"), "{filtered}");
        assert!(filtered.contains("1/1"), "{filtered}");
    }

    #[test]
    fn typed_test_resolves_paths_from_the_tui_workspace() {
        let workspace = std::path::PathBuf::from("/test-ws");
        let mut app = plugin_app(&workspace);
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "plugin test .orca/python-example", &worker, 100);

        match rx.try_recv() {
            Ok(WorkerCmd::TestPlugin { path }) => {
                assert_eq!(path, workspace.join(".orca/python-example"));
            }
            _ => panic!("expected a bounded plugin test worker command"),
        }
    }

    #[test]
    fn typed_uninstall_warns_that_loaded_tools_survive_until_exit() {
        let workspace = std::path::PathBuf::from("/test-ws");
        let mut app = plugin_app(&workspace);
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_plugin("remove-me", workspace.join("remove-me"), true).unwrap();

        slash_command(&mut app, "plugin uninstall remove-me", &worker, 100);

        let output = printed(&app);
        assert!(output.contains("uninstalled plugin remove-me"), "{output}");
        assert!(
            output.contains(
                "loaded plugin skills, tools, and hooks remain available until this TUI exits"
            ),
            "{output}"
        );
        assert!(crate::config::stored_plugin("remove-me").is_none());
    }

    #[test]
    fn picker_test_action_sends_the_selected_registered_root() {
        let workspace = std::path::PathBuf::from("/test-ws");
        let root = workspace.join("alpha");
        let mut app = plugin_app(&workspace);
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.overlay = Some(Overlay::Plugins {
            entries: vec![crate::config::RegisteredPlugin {
                name: "alpha-plugin".into(),
                root: root.clone(),
                enabled: true,
            }],
            filter: String::new(),
            picker: ListPicker::new(1).actions(crate::tui::commands::PLUGIN_ACTIONS),
        });

        press(&mut app, &worker, KeyCode::Char(' '));
        assert!(overlay_text(&app).contains("[x] test"));
        press(&mut app, &worker, KeyCode::Char('x'));

        match rx.try_recv() {
            Ok(WorkerCmd::TestPlugin { path }) => assert_eq!(path, root),
            _ => panic!("expected the selected plugin to be tested"),
        }
        assert!(matches!(app.overlay, Some(Overlay::Plugins { .. })));
    }

    #[test]
    fn typed_init_and_static_validate_use_the_tui_workspace() {
        let workspace = std::env::temp_dir().join(format!(
            "orca-tui-plugin-init-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(&workspace).unwrap();
        let mut app = plugin_app(&workspace);
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "plugin init tui-example --py", &worker, 100);
        let plugin = workspace.join("tui-example");
        assert!(plugin.join("plugin.json").is_file());
        assert!(plugin.join("src/tui_example/server.py").is_file());
        assert!(plugin.join("skills/.gitkeep").is_file());
        assert!(printed(&app).contains("created plugin scaffold"));

        slash_command(&mut app, "plugin validate tui-example", &worker, 100);
        assert!(printed(&app).contains("valid plugin: tui-example"));
        assert!(
            rx.try_recv().is_err(),
            "static commands do not use the worker"
        );
        let _ = std::fs::remove_dir_all(workspace);
    }
}
