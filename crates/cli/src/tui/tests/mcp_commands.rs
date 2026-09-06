mod mcp_command_tests {
    use super::*;

    fn mcp_app() -> App {
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

    fn printed(app: &App) -> String {
        app.pending_history
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(app: &mut App, tx: &mpsc::UnboundedSender<WorkerCmd>, code: KeyCode) {
        handle_terminal_event(
            app,
            CtEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            tx,
            80,
        );
    }

    /// The overlay as drawn, one string per line.
    fn overlay_text(app: &App) -> String {
        live_lines(app, 120)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn picker_cells<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
        let line = text
            .lines()
            .find(|line| {
                let mut cells = line.split_whitespace();
                matches!(cells.next(), Some(first) if first == name || (first == crate::view::glyphs::glyphs().cursor && cells.next() == Some(name)))
            })
            .unwrap_or_else(|| panic!("missing {name} row: {text}"));
        line.split_whitespace()
            .filter(|cell| *cell != crate::view::glyphs::glyphs().cursor)
            .collect()
    }

    #[tokio::test]
    async fn add_list_and_remove_round_trip_through_config_and_reload() {
        let mut app = mcp_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // Bare form with nothing configured points at the add syntax
        // rather than opening an overlay with no rows to toggle.
        slash_command(&mut app, "mcp", &worker, 80);
        assert!(printed(&app).contains("no MCP servers configured"));
        assert!(app.overlay.is_none());

        // Add saves the command verbatim (arguments included) and asks
        // the worker to reconnect.
        slash_command(
            &mut app,
            "mcp add docs npx -y some-server /tmp",
            &worker,
            80,
        );
        let stored = crate::config::stored_mcp_servers();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "docs");
        assert_eq!(stored[0].command, "npx -y some-server /tmp");
        assert!(stored[0].enabled, "a new server starts on");
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadMcp)));
        assert!(printed(&app).contains("mcp server docs added"));

        // Bare form now opens the picker, listing state and command.
        // The count is "…" until a reload reports one.
        slash_command(&mut app, "mcp", &worker, 80);
        assert!(matches!(app.overlay, Some(Overlay::Mcp { .. })));
        let text = overlay_text(&app);
        assert!(text.contains("space toggle"), "{text}");
        assert_eq!(&picker_cells(&text, "docs")[..3], ["docs", "on", "…"]);
        assert!(text.contains("npx -y some-server /tmp"), "{text}");
        press(&mut app, &worker, KeyCode::Esc);

        // Remove drops it and reconnects; "rm" and "delete" are aliases.
        slash_command(&mut app, "mcp remove docs", &worker, 80);
        assert!(crate::config::stored_mcp_servers().is_empty());
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadMcp)));
        assert!(printed(&app).contains("mcp server docs removed"));
    }

    /// Space toggles the selected row: the config is written, the
    /// worker is asked to reconnect, and the overlay stays open showing
    /// the new state at once.
    #[tokio::test]
    async fn space_toggles_the_selected_server_and_the_overlay_stays_open() {
        let mut app = mcp_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server("docs", "run docs").unwrap();
        crate::config::save_mcp_server("fetch", "run fetch").unwrap();

        slash_command(&mut app, "mcp", &worker, 80);
        // Config order is the map's: docs, then fetch.
        press(&mut app, &worker, KeyCode::Down);
        press(&mut app, &worker, KeyCode::Char(' '));

        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadMcp)));
        assert!(
            matches!(app.overlay, Some(Overlay::Mcp { .. })),
            "toggling keeps the overlay open for the next row"
        );
        let stored = crate::config::stored_mcp_servers();
        assert!(stored[0].enabled, "the unselected row is untouched");
        assert!(!stored[1].enabled, "fetch is now off");

        // The row redraws immediately, without waiting for the reload,
        // and an off server shows no tool count.
        let text = overlay_text(&app);
        assert_eq!(&picker_cells(&text, "docs")[..3], ["docs", "on", "…"]);
        assert_eq!(&picker_cells(&text, "fetch")[..2], ["fetch", "off"]);
        assert!(!picker_cells(&text, "fetch").contains(&"…"), "{text}");

        // Enter toggles too, matching /extensions muscle memory.
        press(&mut app, &worker, KeyCode::Enter);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ReloadMcp)));
        assert!(crate::config::stored_mcp_servers()[1].enabled);
        let text = overlay_text(&app);
        assert_eq!(&picker_cells(&text, "fetch")[..3], ["fetch", "on", "…"]);

        press(&mut app, &worker, KeyCode::Esc);
        assert!(app.overlay.is_none());
    }

    #[test]
    fn mcp_picker_filters_by_typing_and_reports_position() {
        let mut app = mcp_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server("alpha", "run alpha").unwrap();
        crate::config::save_mcp_server("beta", "run beta").unwrap();

        slash_command(&mut app, "mcp", &worker, 80);
        assert!(overlay_text(&app).contains("1/2"));
        press(&mut app, &worker, KeyCode::Char('b'));
        let text = overlay_text(&app);
        assert!(text.contains("filter: b"), "{text}");
        assert!(text.contains("beta"), "{text}");
        assert!(!text.contains("run alpha"), "{text}");
        assert!(text.contains("1/1"), "{text}");

        press(&mut app, &worker, KeyCode::Backspace);
        assert!(overlay_text(&app).contains("1/2"));
        crate::config::remove_mcp_server("alpha").unwrap();
        crate::config::remove_mcp_server("beta").unwrap();
    }

    #[test]
    fn plugin_server_rows_are_visible_filterable_and_read_only() {
        let mut app = mcp_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.overlay = Some(Overlay::Mcp {
            entries: vec![crate::tui::mcp_picker::Entry::Plugin(
                crate::mcp::view::PluginMcpEntry {
                    id: "plugin__release_tools__notes".into(),
                    plugin: "release-tools".into(),
                    server: "notes".into(),
                },
            )],
            filter: String::new(),
            picker: ListPicker::new(1),
        });

        let text = overlay_text(&app);
        assert_eq!(
            &picker_cells(&text, "release-tools/notes")[..3],
            ["release-tools/notes", "plugin", "…"]
        );
        assert!(text.contains("plugin rows read-only"), "{text}");
        assert!(text.contains("manage with /plugin"), "{text}");

        press(&mut app, &worker, KeyCode::Char('r'));
        assert!(overlay_text(&app).contains("filter: r"));
        press(&mut app, &worker, KeyCode::Backspace);
        press(&mut app, &worker, KeyCode::Enter);

        assert!(app.overlay.is_none());
        assert!(rx.try_recv().is_err(), "plugin row must not reload MCP");
        assert!(printed(&app).contains("read-only here; manage it with /plugin"));
    }

    /// Tool counts and connection errors come off the shared handle the
    /// worker reloads. The reason goes after the command so a long error
    /// never truncates the row before the command is visible.
    #[tokio::test]
    async fn rows_report_tool_counts_and_connection_errors() {
        let mut app = mcp_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server("ghost", "orca-no-such-binary-xyz").unwrap();
        app.cfg.mcp.reload().await;

        slash_command(&mut app, "mcp", &worker, 80);
        let text = overlay_text(&app);
        assert_eq!(&picker_cells(&text, "ghost")[..3], ["ghost", "on", "failed"]);
        assert!(
            text.contains("failed     orca-no-such-binary-xyz"),
            "the command survives the error: {text}"
        );
        assert!(text.contains("spawn failed"), "the reason is shown: {text}");
    }

    /// Env references survive redaction — they name a variable, they do
    /// not carry it — so the row still says which one a server needs.
    #[test]
    fn redaction_keeps_env_references_and_the_rest_of_the_command() {
        let command = "npx -y mcp-remote https://api.githubcopilot.com/mcp/readonly \
                       --header Authorization:${AUTH_HEADER}";
        assert_eq!(redact_command(command), command);
    }

    #[test]
    fn redaction_masks_literal_credentials_in_every_shape() {
        // The shape a user actually produces: `--header` values cannot
        // contain spaces (the command is whitespace-split), so a pasted
        // credential arrives glued to the header name. The name survives
        // so the row still says what is being sent.
        assert_eq!(
            redact_command("npx mcp-remote https://x.dev/mcp --header Authorization:ghp_realtoken"),
            "npx mcp-remote https://x.dev/mcp --header Authorization:<redacted>"
        );
        // Being an Authorization value is enough on its own — the value
        // need not look token-shaped.
        assert_eq!(
            redact_command("x --header Authorization:Bearer"),
            "x --header Authorization:<redacted>"
        );
        // An unrecognized header name masks the whole word rather than
        // guessing which half is the secret; losing the name is the safe
        // direction.
        assert_eq!(
            redact_command("x --header X-Custom-Auth:ghp_realtoken"),
            "x --header <redacted>"
        );
        // Flag and value in one word.
        assert_eq!(
            redact_command("some-server --api-key=sk-abc123"),
            "some-server --api-key=<redacted>"
        );
        // Flag and value split across words.
        assert_eq!(
            redact_command("some-server --token sk-abc123"),
            "some-server --token <redacted>"
        );
        // A bare token as a positional argument.
        assert_eq!(
            redact_command("some-server github_pat_11ABCDE"),
            "some-server <redacted>"
        );
        // Credentials inside the URL: query parameter and userinfo.
        assert_eq!(
            redact_command("npx mcp-remote https://x.dev/sse?api_key=abc123&mode=fast"),
            "npx mcp-remote https://x.dev/sse?api_key=<redacted>&mode=fast"
        );
        assert_eq!(
            redact_command("npx mcp-remote https://user:hunter2@x.dev/mcp"),
            "npx mcp-remote https://user:<redacted>@x.dev/mcp"
        );
    }

    /// Redaction must not chew up ordinary commands: no flags, no
    /// tokens, nothing that merely looks like one.
    #[test]
    fn redaction_leaves_ordinary_commands_alone() {
        for command in [
            "uvx mcp-server-fetch",
            "npx -y @modelcontextprotocol/server-everything",
            "npx -y mcp-remote https://mcp.context7.com/mcp",
            "npx -y mcp-remote https://gitmcp.io/okikorg/orca",
            "github-mcp-server stdio --toolsets repos,issues",
        ] {
            assert_eq!(redact_command(command), command, "mangled: {command}");
        }
    }

    /// The overlay renders the redacted form, never the stored one.
    #[tokio::test]
    async fn the_picker_never_renders_a_literal_token() {
        let mut app = mcp_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server(
            "github",
            "npx -y mcp-remote https://x.dev/mcp --header Authorization:ghp_supersecret",
        )
        .unwrap();

        slash_command(&mut app, "mcp", &worker, 80);
        let text = overlay_text(&app);
        assert!(!text.contains("ghp_supersecret"), "token on screen: {text}");
        assert!(text.contains("Authorization:<redacted>"), "{text}");
        // The config keeps the real value — this is display-only.
        assert!(crate::config::stored_mcp_servers()[0]
            .command
            .contains("ghp_supersecret"));
    }

    /// The sequence the overlay exists for: toggle on, the row shows `…`
    /// while the reconnect runs, and the count lands once it reports.
    #[tokio::test]
    async fn a_toggled_on_server_moves_from_the_placeholder_to_its_state() {
        let mut app = mcp_app();
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::config::save_mcp_server("ghost", "orca-no-such-binary-xyz").unwrap();
        crate::config::set_mcp_enabled("ghost", false).unwrap();
        app.cfg.mcp.reload().await;

        slash_command(&mut app, "mcp", &worker, 80);
        let text = overlay_text(&app);
        assert_eq!(&picker_cells(&text, "ghost")[..2], ["ghost", "off"]);

        // Toggling on redraws as on with no state yet; the worker has
        // not reconnected.
        press(&mut app, &worker, KeyCode::Char(' '));
        let text = overlay_text(&app);
        assert_eq!(&picker_cells(&text, "ghost")[..3], ["ghost", "on", "…"]);

        // The worker's reload resolves it, with the overlay still open.
        app.cfg.mcp.reload().await;
        let text = overlay_text(&app);
        assert!(!text.contains('…'), "the placeholder resolves: {text}");
        assert_eq!(&picker_cells(&text, "ghost")[..3], ["ghost", "on", "failed"]);
    }

    #[tokio::test]
    async fn bad_input_reports_and_sends_nothing() {
        let mut app = mcp_app();
        let (worker, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // add needs both a name and a command.
        slash_command(&mut app, "mcp add", &worker, 80);
        slash_command(&mut app, "mcp add docs", &worker, 80);
        assert!(printed(&app).contains("usage: /mcp"));

        // Names feed the model-facing tool prefix, so junk is rejected.
        slash_command(&mut app, "mcp add bad/name run it", &worker, 80);
        assert!(printed(&app).contains("invalid server name: bad/name"));

        // Removing something that was never added names the valid set.
        slash_command(&mut app, "mcp remove nope", &worker, 80);
        assert!(printed(&app).contains("unknown mcp server: nope"));

        // Re-adding a server the user turned off edits it without
        // enabling it, and says so rather than claiming to connect.
        crate::config::save_mcp_server("docs", "run docs").unwrap();
        crate::config::set_mcp_enabled("docs", false).unwrap();
        slash_command(&mut app, "mcp add docs run other", &worker, 80);
        let stored = crate::config::stored_mcp_servers();
        assert_eq!(stored[0].command, "run other");
        assert!(!stored[0].enabled, "an edit is not an enable");
        assert!(printed(&app).contains("mcp server docs updated — still off"));
        crate::config::remove_mcp_server("docs").unwrap();
        while rx.try_recv().is_ok() {}

        slash_command(&mut app, "mcp frobnicate", &worker, 80);
        assert!(printed(&app).contains("usage: /mcp"));

        assert!(crate::config::stored_mcp_servers().is_empty());
        assert!(rx.try_recv().is_err(), "bad input sends nothing");
    }
}
