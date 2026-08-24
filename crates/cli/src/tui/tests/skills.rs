#[cfg(test)]
mod skills_command_tests {
    use super::*;

    /// A temp tree plus the `Skills` handle that scans it. Roots are
    /// passed in explicitly, so a test never reaches the developer's own
    /// ~/.claude/skills.
    struct Fixture {
        dir: std::path::PathBuf,
        skills: crate::skills::Skills,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "orca-tui-skills-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let skills = crate::skills::Skills::new(&dir, None, None);
            Self { dir, skills }
        }

        fn skill(&self, name: &str, description: &str) {
            let path = self.dir.join(".orca/skills").join(name).join("SKILL.md");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                format!("---\nname: {name}\ndescription: {description}\n---\n\nstep one\n"),
            )
            .unwrap();
            self.skills.reload();
        }

        fn app(&self) -> App {
            App::new(TuiConfig {
                model_name: "m".into(),
                workspace_name: "w".into(),
                workspace_root: "/test-ws".into(),
                provider: Provider::Local,
                subagent_depth: orca_harness_tools::SubagentDepth::new(1),
                stats: orca_harness_tools::BackgroundStats::new(),
                session_id: None,
                mcp: Default::default(),
                skills: self.skills.clone(),
                mode: Default::default(),
                todos: Default::default(),
                plan: Default::default(),
            })
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn printed(app: &App) -> String {
        app.pending_history
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn overlay_text(app: &App) -> String {
        live_lines(app, 120)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Nothing found: the bare form says how to get one instead of
    /// opening an overlay with no rows in it.
    #[test]
    fn empty_catalog_points_at_add_and_create() {
        let fixture = Fixture::new("empty");
        let mut app = fixture.app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills", &worker, 80);
        assert!(app.overlay.is_none());
        let text = printed(&app);
        assert!(text.contains("/skills add"), "{text}");
        assert!(text.contains("/skills create"), "{text}");
        assert!(text.contains(".claude/skills"), "{text}");
    }

    fn press(app: &mut App, worker: &mpsc::UnboundedSender<WorkerCmd>, code: KeyCode) {
        handle_terminal_event(
            app,
            CtEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            worker,
            80,
        );
    }

    /// Space reveals the strip rather than acting, so neither toggling
    /// nor deleting is one stray keystroke away.
    #[test]
    fn space_reveals_the_actions_and_t_toggles() {
        let fixture = Fixture::new("toggle");
        fixture.skill("release", "Cut a release");
        let mut app = fixture.app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills", &worker, 80);
        let shown = overlay_text(&app);
        assert!(shown.contains("release"), "{shown}");
        assert!(shown.contains("Cut a release"), "{shown}");
        assert!(shown.contains("space actions"), "{shown}");

        press(&mut app, &worker, KeyCode::Char(' '));
        let armed = overlay_text(&app);
        assert!(armed.contains("[t] toggle"), "{armed}");
        assert!(armed.contains("[d] delete"), "{armed}");
        assert_eq!(
            crate::config::stored_skill_enabled("release"),
            None,
            "space itself changes nothing"
        );

        press(&mut app, &worker, KeyCode::Char('t'));
        assert_eq!(
            crate::config::stored_skill_enabled("release"),
            Some(false),
            "the action key writes the override"
        );
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadSkills)));
        // The overlay stays open and the row redraws off at once, while
        // the rescan and rebuild run behind it.
        assert!(app.overlay.is_some());
        assert!(overlay_text(&app).contains("off"), "{}", overlay_text(&app));

        // Enter keeps the one-key path for the common case.
        press(&mut app, &worker, KeyCode::Enter);
        assert_eq!(crate::config::stored_skill_enabled("release"), Some(true));
    }

    #[test]
    fn skills_picker_filters_by_typing_and_backspace() {
        let fixture = Fixture::new("filter");
        fixture.skill("deploy", "Ship the application");
        fixture.skill("review", "Review a change");
        let mut app = fixture.app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills", &worker, 80);
        assert!(overlay_text(&app).contains("1/2"));
        press(&mut app, &worker, KeyCode::Char('r'));
        let text = overlay_text(&app);
        assert!(text.contains("filter: r"), "{text}");
        assert!(text.contains("review"), "{text}");
        assert!(!text.contains("deploy"), "{text}");
        assert!(text.contains("1/1"), "{text}");

        press(&mut app, &worker, KeyCode::Backspace);
        assert!(overlay_text(&app).contains("1/2"));
    }

    /// Delete removes the folder, the row, and the saved override — and
    /// only for skills this host installed.
    #[test]
    fn delete_action_removes_an_installed_skill() {
        let fixture = Fixture::new("delete");
        fixture.skill("release", "Cut a release");
        let mut app = fixture.app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = fixture.dir.join(".orca/skills/release");
        assert!(dir.is_dir());

        slash_command(&mut app, "skills", &worker, 80);
        press(&mut app, &worker, KeyCode::Char(' '));
        press(&mut app, &worker, KeyCode::Char('d'));

        assert!(!dir.exists(), "the folder is gone");
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadSkills)));
        // Last row deleted: the overlay closes rather than showing an
        // empty list.
        assert!(app.overlay.is_none());
        assert!(
            printed(&app).contains("removed release"),
            "{}",
            printed(&app)
        );
    }

    /// The whole loop against the real internet: install a published
    /// skill from GitHub through `/skills add`, confirm the running
    /// agent is offered it, then delete it from the overlay. Ignored by
    /// default — it clones a repository and spawns the built binary
    /// against a local model endpoint.
    ///
    /// Run with:
    /// `cargo test -p orcacode --bin orcacode e2e_ -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "network: clones a github repository and calls a model"]
    async fn e2e_add_use_and_remove_a_published_skill() {
        let fixture = Fixture::new("e2e");
        let config = fixture.dir.join("config");
        let workspace = fixture.dir.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("config.json"),
            r#"{"provider": "local", "models": {"local": "gemma4:e2b-mlx"}}"#,
        )
        .unwrap();
        let skills = crate::skills::Skills::new(&workspace, Some(config.clone()), None);
        let mut app = App::new(TuiConfig {
            model_name: "e2e".into(),
            workspace_name: "ws".into(),
            workspace_root: workspace.display().to_string(),
            provider: Provider::Local,
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
            session_id: None,
            mcp: Default::default(),
            skills: skills.clone(),
            mode: Default::default(),
            todos: Default::default(),
            plan: Default::default(),
        });
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // 1. Add, exactly as the composer would.
        slash_command(
            &mut app,
            "skills add vercel-labs/agent-skills --skill writing-guidelines",
            &worker,
            80,
        );
        let Ok(WorkerCmd::InstallSkill { source, here }) = rx.try_recv() else {
            panic!("no install command: {}", printed(&app));
        };
        assert!(!here, "installs beside config.json by default");
        // What the worker does with it.
        let lines = skills.add(&source, here).await.expect("install");
        println!("{}", lines.join("\n"));
        skills.reload();
        let installed = config.join("skills/writing-guidelines/SKILL.md");
        assert!(installed.is_file(), "SKILL.md landed at {installed:?}");

        // 2. The running agent is offered it, by name, with its blurb.
        let tool = skills.tool().expect("a skill tool");
        let schema = tool.schema();
        assert_eq!(schema.name, "skill");
        assert!(
            schema.description.contains("writing-guidelines"),
            "{}",
            schema.description
        );

        // 3. A real model, given the real binary, calls it. The tool log
        //    goes to stderr, so that is where the call shows up.
        // The test binary lives in target/<profile>/deps/, so the CLI
        // it was built alongside is two directories up.
        let test_binary = std::env::current_exe().expect("test binary path");
        let binary = test_binary
            .parent()
            .and_then(std::path::Path::parent)
            .expect("target dir")
            .join("orcacode");
        assert!(binary.is_file(), "build the binary first: {binary:?}");
        let run = std::process::Command::new(&binary)
            .env("ORCA_CONFIG_DIR", &config)
            .args(["--workspace"])
            .arg(&workspace)
            .args([
                "--no-session",
                "--auto-approve",
                "--max-steps",
                "4",
                "-p",
                "Load the writing-guidelines skill and quote its first heading. \
                 Use the skill tool.",
            ])
            .output()
            .expect("run orcacode");
        let stderr = String::from_utf8_lossy(&run.stderr);
        let stdout = String::from_utf8_lossy(&run.stdout);
        println!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
        assert!(
            stderr.contains("skill"),
            "the model never reached for the skill tool"
        );

        // 4. Remove it from the overlay: space reveals, d deletes.
        slash_command(&mut app, "skills", &worker, 80);
        let shown = overlay_text(&app);
        assert!(shown.contains("writing-guidelines"), "{shown}");
        press(&mut app, &worker, KeyCode::Char(' '));
        press(&mut app, &worker, KeyCode::Char('d'));
        assert!(
            !config.join("skills/writing-guidelines").exists(),
            "the folder is gone"
        );
        skills.reload();
        assert!(skills.tool().is_none(), "and so is the tool");
    }

    /// A skill from a compatibility root is not this host's to delete.
    #[test]
    fn delete_refuses_a_skill_from_a_root_it_does_not_own() {
        let fixture = Fixture::new("foreign");
        let path = fixture.dir.join(".claude/skills/borrowed/SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nname: borrowed\ndescription: someone else's\n---\n\nbody\n",
        )
        .unwrap();
        fixture.skills.reload();
        let mut app = fixture.app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills remove borrowed", &worker, 80);
        assert!(path.is_file(), "the file must survive");
        let text = printed(&app);
        assert!(text.contains("only deletes what it installed"), "{text}");
    }

    #[test]
    fn show_reports_one_skill_and_rejects_unknown_names() {
        let fixture = Fixture::new("show");
        fixture.skill("release", "Cut a release");
        let mut app = fixture.app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills show release", &worker, 80);
        let text = printed(&app);
        assert!(text.contains(".orca/skills"), "{text}");
        assert!(text.contains("Cut a release"), "{text}");

        slash_command(&mut app, "skills show nope", &worker, 80);
        let text = printed(&app);
        assert!(text.contains("unknown skill: nope"), "{text}");
        assert!(text.contains("found: release"), "{text}");
    }

    #[test]
    fn reload_goes_through_the_worker_and_garbage_is_rejected() {
        let fixture = Fixture::new("reload");
        let mut app = fixture.app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "skills reload", &worker, 80);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadSkills)));
        assert!(printed(&app).contains("rescanning skills"));

        slash_command(&mut app, "skills wat", &worker, 80);
        assert!(rx.try_recv().is_err(), "no command for a bad argument");
        assert!(printed(&app).contains("unknown /skills argument: wat"));
    }

}
