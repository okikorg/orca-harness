#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::plugins::*;

    fn raw() -> String {
        TEST_FILE
            .with(|file| file.borrow().clone())
            .expect("config written")
    }

    fn seed(body: &str) {
        TEST_FILE.with(|file| *file.borrow_mut() = Some(body.to_string()));
    }

    #[test]
    fn subagent_preferences_round_trip_and_preserve_other_config() {
        let saved = orca_harness_tools::SubagentDepth::new(3);
        saved.set_max_steps(36);
        saved.set_timeout_secs(600);
        saved.set_output_chars(16_000);
        saved.set_tool_attempts(5);
        saved.set_retry_backoff_ms(500);
        save_key("openai", "sk-1").unwrap();
        save_subagent_settings(&saved).unwrap();

        let loaded = orca_harness_tools::SubagentDepth::new(1);
        load_subagent_settings(&loaded);

        assert_eq!(loaded.get(), 3);
        assert_eq!(loaded.max_steps(), 36);
        assert_eq!(loaded.timeout_secs(), 600);
        assert_eq!(loaded.output_chars(), 16_000);
        assert_eq!(loaded.tool_attempts(), 5);
        assert_eq!(loaded.retry_backoff_ms(), 500);
        assert_eq!(stored_key("openai").as_deref(), Some("sk-1"));
    }

    #[test]
    fn save_then_load_round_trips_per_provider() {
        assert_eq!(stored_key("openrouter"), None);
        save_key("openrouter", "sk-or-123").unwrap();
        save_key("openai", "sk-456").unwrap();

        assert_eq!(stored_key("openrouter").as_deref(), Some("sk-or-123"));
        assert_eq!(stored_key("openai").as_deref(), Some("sk-456"));
        // Overwriting replaces, and never clobbers the other provider.
        save_key("openrouter", "sk-or-789").unwrap();
        assert_eq!(stored_key("openrouter").as_deref(), Some("sk-or-789"));
        assert_eq!(stored_key("openai").as_deref(), Some("sk-456"));
    }

    #[test]
    fn provider_theme_and_models_round_trip_alongside_keys() {
        assert_eq!(stored_provider(), None);
        assert_eq!(stored_theme(), None);
        assert_eq!(stored_model("openrouter"), None);

        save_key("openrouter", "sk-or-1").unwrap();
        save_provider("openrouter").unwrap();
        save_theme("nord").unwrap();
        save_model("openrouter", "anthropic/claude-sonnet-4").unwrap();
        save_model("local", "qwen3.5:9b").unwrap();

        assert_eq!(stored_provider().as_deref(), Some("openrouter"));
        assert_eq!(stored_theme().as_deref(), Some("nord"));
        assert_eq!(
            stored_model("openrouter").as_deref(),
            Some("anthropic/claude-sonnet-4")
        );
        assert_eq!(stored_model("local").as_deref(), Some("qwen3.5:9b"));
        // Preferences never disturb the keys section.
        assert_eq!(stored_key("openrouter").as_deref(), Some("sk-or-1"));
    }

    #[test]
    fn save_preserves_unknown_fields() {
        seed(r#"{"future_field": true, "api_keys": {"openai": "sk-old"}}"#);
        save_key("openrouter", "sk-or-new").unwrap();
        save_theme("mono").unwrap();

        let root: Value = serde_json::from_str(&raw()).unwrap();
        assert_eq!(root["future_field"], true);
        assert_eq!(root["api_keys"]["openai"], "sk-old");
        assert_eq!(root["api_keys"]["openrouter"], "sk-or-new");
        assert_eq!(root["theme"], "mono");
    }

    #[test]
    fn approvals_are_scoped_per_workspace_and_removable() {
        assert!(stored_approvals("/repo/a").is_empty());

        save_approval("/repo/a", "shell").unwrap();
        save_approval("/repo/a", "write_file").unwrap();
        save_approval("/repo/a", "shell").unwrap(); // idempotent
        save_approval("/repo/b", "pykernel").unwrap();

        assert_eq!(stored_approvals("/repo/a"), ["shell", "write_file"]);
        // Trust does not leak across workspaces.
        assert_eq!(stored_approvals("/repo/b"), ["pykernel"]);

        remove_approval("/repo/a", "shell").unwrap();
        assert_eq!(stored_approvals("/repo/a"), ["write_file"]);
        assert_eq!(stored_approvals("/repo/b"), ["pykernel"]);
        // Approvals coexist with the other sections.
        save_key("openai", "sk-1").unwrap();
        assert_eq!(stored_approvals("/repo/a"), ["write_file"]);
    }

    #[test]
    fn extension_overrides_round_trip_and_coexist() {
        assert_eq!(stored_extension("retry"), None);
        save_extension("retry", true).unwrap();
        save_extension("truncation", false).unwrap();
        assert_eq!(stored_extension("retry"), Some(true));
        assert_eq!(stored_extension("truncation"), Some(false));
        // Overwriting flips just the one entry.
        save_extension("retry", false).unwrap();
        assert_eq!(stored_extension("retry"), Some(false));
        assert_eq!(stored_extension("truncation"), Some(false));
        // Non-boolean garbage reads as unset, not as a state.
        seed(r#"{"extensions": {"retry": "yes"}}"#);
        assert_eq!(stored_extension("retry"), None);
    }

    #[test]
    fn skill_overrides_default_on_and_round_trip() {
        // Nothing saved reads as "no override", which callers treat as on.
        assert_eq!(stored_skill_enabled("release"), None);

        save_skill_enabled("release", false).unwrap();
        save_skill_enabled("review", true).unwrap();
        assert_eq!(stored_skill_enabled("release"), Some(false));
        assert_eq!(stored_skill_enabled("review"), Some(true));

        // Re-enabling flips just the one entry, and skills coexist with
        // the other sections.
        save_skill_enabled("release", true).unwrap();
        save_key("openai", "sk-1").unwrap();
        assert_eq!(stored_skill_enabled("release"), Some(true));
        assert_eq!(stored_key("openai").as_deref(), Some("sk-1"));

        // Non-boolean garbage reads as unset, not as a state.
        seed(r#"{"skills": {"release": "off"}}"#);
        assert_eq!(stored_skill_enabled("release"), None);
    }

    /// A (name, command, enabled) view of the stored servers.
    #[cfg(test)]
    fn listed() -> Vec<(String, String, bool)> {
        stored_mcp_servers()
            .into_iter()
            .map(|s| (s.name, s.command, s.enabled))
            .collect()
    }

    #[test]
    fn mcp_servers_round_trip_and_are_removable() {
        assert!(stored_mcp_servers().is_empty());

        save_mcp_server("docs", "npx -y some-server /tmp").unwrap();
        save_mcp_server("db", "uvx db-server").unwrap();
        assert_eq!(
            listed(),
            [
                ("db".to_string(), "uvx db-server".to_string(), true),
                (
                    "docs".to_string(),
                    "npx -y some-server /tmp".to_string(),
                    true
                ),
            ]
        );

        // Re-adding a name replaces its command.
        save_mcp_server("docs", "npx -y other-server").unwrap();
        assert_eq!(listed()[1].1, "npx -y other-server");

        remove_mcp_server("docs").unwrap();
        assert_eq!(stored_mcp_servers().len(), 1);
        // Removing an unknown name is a no-op, not an error.
        remove_mcp_server("nope").unwrap();
        assert_eq!(stored_mcp_servers().len(), 1);
        // Servers coexist with the other sections.
        save_key("openai", "sk-1").unwrap();
        assert_eq!(stored_mcp_servers().len(), 1);

        // Blank or unusable commands read as absent, not as servers:
        // a blank string, a number, an object with no command, and an
        // object whose command is blank.
        seed(
            r#"{"mcp": {
                "a": "  ",
                "b": 7,
                "c": "run c",
                "d": {"enabled": true},
                "e": {"command": " "},
                "f": {"command": "run f", "enabled": false}
            }}"#,
        );
        assert_eq!(
            listed(),
            [
                ("c".to_string(), "run c".to_string(), true),
                ("f".to_string(), "run f".to_string(), false),
            ]
        );
    }

    #[test]
    fn plugins_round_trip_in_name_order_and_preserve_unrelated_config() {
        seed(r#"{"future": {"kept": true}}"#);
        save_plugin("zeta", "/plugins/zeta", false).unwrap();
        save_plugin("alpha", "/plugins/alpha", true).unwrap();

        let plugins = stored_plugins();
        assert_eq!(
            plugins,
            [
                RegisteredPlugin {
                    name: "alpha".into(),
                    root: PathBuf::from("/plugins/alpha"),
                    enabled: true,
                },
                RegisteredPlugin {
                    name: "zeta".into(),
                    root: PathBuf::from("/plugins/zeta"),
                    enabled: false,
                },
            ]
        );
        assert_eq!(
            serde_json::from_str::<Value>(&raw()).unwrap()["future"]["kept"],
            true
        );
    }

    #[test]
    fn malformed_plugin_entries_are_skipped_and_valid_entries_can_change_state() {
        seed(
            r#"{"plugins": {
                "missing-root": {"enabled": true},
                "bad-root": {"root": 7, "enabled": false},
                "bad-enabled": {"root": "/plugins/bad", "enabled": "yes"},
                "relative-root": {"root": "plugins/relative", "enabled": false},
                "../escape": {"root": "/plugins/escape", "enabled": true},
                "valid": {"root": "/plugins/valid", "enabled": false}
            }}"#,
        );
        assert_eq!(stored_plugins().len(), 1);
        assert_eq!(stored_plugin("valid").unwrap().name, "valid");
        assert!(stored_plugin("missing-root").is_none());

        assert!(set_plugin_enabled("valid", true).unwrap());
        assert!(stored_plugin("valid").unwrap().enabled);
        assert!(!set_plugin_enabled("unknown", true).unwrap());
        assert!(remove_plugin("valid").unwrap());
        assert!(!remove_plugin("valid").unwrap());
        assert!(stored_plugins().is_empty());
    }

    #[test]
    fn enabling_a_plugin_atomically_canonicalizes_root_and_preserves_unknown_fields() {
        seed(
            r#"{"future":true,"plugins":{"valid":{
                "root":"/plugins/old/../valid","enabled":false,"futureEntry":"kept"
            }}}"#,
        );

        assert!(enable_plugin("valid", Path::new("/plugins/valid")).unwrap());

        let saved: Value = serde_json::from_str(&raw()).unwrap();
        assert_eq!(saved["future"], true);
        assert_eq!(saved["plugins"]["valid"]["root"], "/plugins/valid");
        assert_eq!(saved["plugins"]["valid"]["enabled"], true);
        assert_eq!(saved["plugins"]["valid"]["futureEntry"], "kept");
    }

    #[test]
    fn plugin_data_path_is_beside_config_under_the_plugin_name() {
        let path = plugin_data_path("rl-tools").unwrap();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("rl-tools")
        );
        assert_eq!(
            path.parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str()),
            Some("plugin-data")
        );
    }

    #[test]
    fn plugin_data_directory_creation_is_explicit_and_yields_a_directory() {
        let root = std::env::temp_dir().join(format!(
            "orcacode-plugin-data-portable-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("plugin-data/example");
        assert!(!path.exists());

        create_plugin_data_dir(&path).unwrap();

        assert!(path.is_dir());
        create_plugin_data_dir(&path).unwrap();
        assert!(path.is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn plugin_data_directory_is_created_lazily_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "orcacode-plugin-data-config-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("plugin-data/example");
        assert!(!path.exists());

        create_plugin_data_dir(&path).unwrap();

        assert!(path.is_dir());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_plugin_data_dir(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn toggling_preserves_the_command_and_promotes_the_string_shape() {
        // The bare-string shape written before toggles existed reads as
        // enabled and survives a disable.
        seed(r#"{"mcp": {"docs": "npx -y some-server"}}"#);
        assert_eq!(
            listed(),
            [("docs".into(), "npx -y some-server".into(), true)]
        );

        set_mcp_enabled("docs", false).unwrap();
        assert_eq!(
            listed(),
            [("docs".to_string(), "npx -y some-server".to_string(), false)]
        );

        // Re-saving a disabled server's command keeps it disabled: an
        // edit is not an enable.
        save_mcp_server("docs", "npx -y other-server").unwrap();
        assert_eq!(
            listed(),
            [("docs".to_string(), "npx -y other-server".to_string(), false)]
        );

        set_mcp_enabled("docs", true).unwrap();
        assert!(listed()[0].2);

        // Toggling a name that is not configured is a no-op, not an
        // error and not a new entry.
        set_mcp_enabled("nope", false).unwrap();
        assert_eq!(stored_mcp_servers().len(), 1);
        seed("{}");
        set_mcp_enabled("nope", false).unwrap();
        assert!(stored_mcp_servers().is_empty());
    }

    #[test]
    fn corrupt_config_is_replaced_not_fatal() {
        seed("not json {");
        assert_eq!(stored_key("openai"), None);
        save_key("openai", "sk-new").unwrap();
        assert_eq!(stored_key("openai").as_deref(), Some("sk-new"));
    }

    /// The real on-disk writer must produce owner-only files, and must
    /// tighten a pre-existing looser file when rewriting it.
    #[cfg(unix)]
    #[test]
    fn config_file_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("orcacode-perm-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        write_private(&path, "{}\n").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config must not be group/world readable");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, "{}\n").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "rewrites tighten a loose pre-existing file");

        let _ = fs::remove_dir_all(&dir);
    }
}
