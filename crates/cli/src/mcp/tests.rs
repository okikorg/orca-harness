#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use orca_harness_core::{CancellationToken, ToolContext};
use orca_harness_tool_extensions::mcp::{ProcessEnvironment, StdioLaunch};
use serde_json::json;

use super::*;

#[path = "tests/hooks.rs"]
mod hook_tests;
#[path = "tests/ordering.rs"]
mod ordering_tests;

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

const SERVER: &str = r#"#!/bin/sh
[ -d "$PLUGIN_DATA" ] || exit 31
[ "$1" = "$PLUGIN_DATA/launch marker" ] || exit 32
[ "$LAUNCH_VALUE" = "structured value" ] || exit 33
[ "$PWD" = "$PLUGIN_ROOT" ] || exit 34
touch "$1"
read _initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}'
read _initialized
read _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echo structured fixture","inputSchema":{"type":"object"}}]}}'
read _call
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"plugin call passed"}]}}'
"#;

const STANDALONE_SERVER: &str = r#"#!/bin/sh
read _initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}'
read _initialized
read _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echo structured fixture","inputSchema":{"type":"object"}}]}}'
read _call
"#;

struct Fixture {
    root: PathBuf,
    data_root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "orcacode-plugin-runtime-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self {
            data_root: root.join("config/plugin-data"),
            root,
        }
    }

    fn plugin(&self, name: &str, servers: &[&str]) -> crate::config::RegisteredPlugin {
        let root = self.root.join(name);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("plugin.json"),
            format!(
                r#"{{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"{name}","description":"runtime fixture"}}"#
            ),
        )
        .unwrap();
        fs::write(root.join("server.sh"), SERVER).unwrap();
        let entries = servers
            .iter()
            .map(|server| {
                format!(
                    r#""{server}":{{"type":"stdio","command":"sh","args":["${{PLUGIN_ROOT}}/server.sh","${{PLUGIN_DATA}}/launch marker"],"env":{{"LAUNCH_VALUE":"structured value"}},"cwd":"${{PLUGIN_ROOT}}"}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        fs::write(
            root.join("mcp.json"),
            format!(
                r#"{{"$schema":"https://agent-plugins.org/schemas/1.0.0/mcp.schema.json","mcpServers":{{{entries}}}}}"#
            ),
        )
        .unwrap();
        crate::config::RegisteredPlugin {
            name: name.into(),
            root,
            enabled: true,
        }
    }

    fn plugin_without_mcp(&self, name: &str) -> crate::config::RegisteredPlugin {
        let plugin = self.plugin(name, &[]);
        fs::remove_file(plugin.root.join("mcp.json")).unwrap();
        plugin
    }

    fn add_deny_hook(&self, plugin: &crate::config::RegisteredPlugin) {
        let extension = plugin.root.join("io.github.okikorg.orcacode");
        fs::create_dir_all(&extension).unwrap();
        fs::write(
            plugin.root.join("hook.sh"),
            "read input\nprintf '%s' '{\"decision\":\"deny\",\"reason\":\"plugin policy\"}'\n",
        )
        .unwrap();
        fs::write(
            extension.join("hooks.json"),
            r#"{"version":1,"hooks":{"before_tool":[{"command":"sh","args":["${PLUGIN_ROOT}/hook.sh"]}]}}"#,
        )
        .unwrap();
    }

    fn replace_mcp(&self, plugin: &crate::config::RegisteredPlugin, servers: &str) {
        fs::write(
            plugin.root.join("mcp.json"),
            format!(
                r#"{{"$schema":"https://agent-plugins.org/schemas/1.0.0/mcp.schema.json","mcpServers":{{{servers}}}}}"#
            ),
        )
        .unwrap();
    }

    fn snapshot(&self, plugins: Vec<crate::config::RegisteredPlugin>) -> PluginSnapshot {
        PluginSnapshot::from_registrations(plugins, |name| Ok(self.data_root.join(name)))
    }

    fn standalone_server(&self, name: &str) -> PathBuf {
        let path = self.root.join(format!("{name}.sh"));
        fs::write(&path, STANDALONE_SERVER).unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn context(name: &str) -> ToolContext {
    ToolContext {
        call_id: "runtime-test".into(),
        tool_name: name.into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

fn tool_names(servers: &McpServers) -> Vec<String> {
    servers
        .tools()
        .iter()
        .map(|tool| tool.schema().name)
        .collect()
}

/// Reload against an empty config clears the set quietly; a
/// misconfigured server reports and is skipped, never fatal.
#[tokio::test]
async fn reload_reports_per_server_and_replaces_the_set() {
    let servers = McpServers::with_plugin_snapshot(PluginSnapshot::empty());
    assert!(servers.reload().await.is_empty());
    assert_eq!(servers.tools().len(), 3);

    crate::config::save_mcp_server("ghost", "orca-no-such-binary-xyz").unwrap();
    let lines = servers.reload().await;
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("MCP ghost ·"), "line: {}", lines[0]);
    assert_eq!(servers.tools().len(), 3);
    assert!(matches!(servers.state("ghost"), Some(McpState::Failed(_))));
}

/// The diff: an unchanged config is a no-op, so toggling one server
/// never re-handshakes the others. Disabling drops the connection;
/// re-enabling retries it.
#[tokio::test]
async fn reload_only_touches_servers_that_changed() {
    let servers = McpServers::with_plugin_snapshot(PluginSnapshot::empty());
    crate::config::save_mcp_server("ghost", "orca-no-such-binary-xyz").unwrap();
    assert_eq!(servers.reload().await.len(), 1);
    assert!(servers.reload().await.is_empty());
    assert!(matches!(servers.state("ghost"), Some(McpState::Failed(_))));

    crate::config::set_mcp_enabled("ghost", false).unwrap();
    assert_eq!(servers.reload().await, ["MCP ghost disconnected"]);
    assert_eq!(servers.state("ghost"), None);

    crate::config::set_mcp_enabled("ghost", true).unwrap();
    assert_eq!(servers.reload().await.len(), 1);
    assert!(matches!(servers.state("ghost"), Some(McpState::Failed(_))));

    crate::config::remove_mcp_server("ghost").unwrap();
    assert!(servers.reload().await.is_empty());
    assert_eq!(servers.state("ghost"), None);
}

#[tokio::test]
async fn enabled_plugin_joins_existing_catalog_and_disabled_plugin_is_absent() {
    let fixture = Fixture::new("visibility");
    let enabled = fixture.plugin("enabled-plugin", &["echo"]);
    let mut disabled = fixture.plugin("disabled-plugin", &["unused"]);
    disabled.enabled = false;
    let disabled_data = fixture.data_root.join("disabled-plugin");
    let enabled_data = fixture.data_root.join("enabled-plugin");
    let servers = McpServers::with_plugin_snapshot(fixture.snapshot(vec![disabled, enabled]));

    assert_eq!(
        servers.plugin_state("enabled-plugin"),
        PluginRuntimeState::Starting
    );
    assert_eq!(
        servers.plugin_state("disabled-plugin"),
        PluginRuntimeState::NotLoaded
    );
    assert!(!enabled_data.exists(), "static load created plugin data");
    let lines = servers.reload().await;
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Plugin enabled-plugin MCP echo connected")),
        "lines: {lines:?}"
    );
    assert!(!lines.iter().any(|line| line.contains("disabled-plugin")));
    assert!(enabled_data.join("launch marker").is_file());
    assert!(!disabled_data.exists());
    assert_eq!(
        servers.plugin_state("enabled-plugin"),
        PluginRuntimeState::Loaded {
            servers: 1,
            tools: 1,
        }
    );
    let inventory = servers.startup_inventory();
    assert_eq!(inventory.plugins, 1);
    assert_eq!(inventory.servers, 1);
    assert_eq!(inventory.tools, 1);
    assert_eq!(inventory.connected_notices.len(), 1);

    let remote = "mcp__plugin__enabled_plugin__echo__echo";
    let names = tool_names(&servers);
    assert_eq!(
        &names[..3],
        ["mcp_search_tools", "mcp_select_tool", "mcp_features"]
    );
    assert!(names.iter().any(|name| name == remote));
    assert!(!names.iter().any(|name| name.contains("disabled_plugin")));

    let tools = servers.tools();
    let search = tools
        .iter()
        .find(|tool| tool.schema().name == "mcp_search_tools")
        .unwrap();
    let found = search
        .call(
            json!({"query": "structured echo"}),
            &context("mcp_search_tools"),
        )
        .await
        .unwrap();
    assert_eq!(found["tools"][0]["name"], remote);
    let fallback = search
        .call(
            json!({"query": "use structured echo plugin please"}),
            &context("mcp_search_tools"),
        )
        .await
        .unwrap();
    assert_eq!(fallback["tools"][0]["name"], remote);
    let select = tools
        .iter()
        .find(|tool| tool.schema().name == "mcp_select_tool")
        .unwrap();
    select
        .call(json!({"name": remote}), &context("mcp_select_tool"))
        .await
        .unwrap();
    let called = tools
        .iter()
        .find(|tool| tool.schema().name == remote)
        .unwrap()
        .call(json!({}), &context(remote))
        .await
        .unwrap();
    assert_eq!(called, json!({"content": "plugin call passed"}));
}

#[tokio::test]
async fn missing_plugin_does_not_block_a_healthy_sibling() {
    let fixture = Fixture::new("broken-isolation");
    let healthy = fixture.plugin("healthy-plugin", &["echo"]);
    let missing = crate::config::RegisteredPlugin {
        name: "moved-plugin".into(),
        root: fixture.root.join("moved-away"),
        enabled: true,
    };
    let servers = McpServers::with_plugin_snapshot(fixture.snapshot(vec![missing, healthy]));

    assert!(matches!(
        servers.plugin_state("moved-plugin"),
        PluginRuntimeState::Failed { error, .. } if error.contains("plugin root")
    ));
    let lines = servers.reload().await;
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("Plugin moved-plugin ·")),
        "lines: {lines:?}"
    );
    assert!(tool_names(&servers)
        .iter()
        .any(|name| name.contains("healthy_plugin")));
    assert!(!fixture.data_root.join("moved-plugin").exists());
}

#[tokio::test]
async fn enabled_plugin_without_stdio_is_loaded_and_never_creates_data() {
    let fixture = Fixture::new("no-stdio");
    let plugin = fixture.plugin_without_mcp("metadata-only");
    fs::create_dir_all(plugin.root.join("skills")).unwrap();
    let data = fixture.data_root.join("metadata-only");
    let servers = McpServers::with_plugin_snapshot(fixture.snapshot(vec![plugin]));

    let lines = servers.reload().await;
    assert!(lines.is_empty(), "lines: {lines:?}");
    assert!(!data.exists());
    assert_eq!(tool_names(&servers).len(), 3);
    assert!(matches!(
        servers.plugin_state("metadata-only"),
        PluginRuntimeState::Loaded {
            servers: 0,
            tools: 0
        }
    ));
}

#[tokio::test]
async fn invalid_only_plugin_reports_degraded_mcp_component() {
    let fixture = Fixture::new("invalid-only");
    let plugin = fixture.plugin("invalid-only", &[]);
    fixture.replace_mcp(&plugin, r#""broken":{"type":"stdio","command":""}"#);
    let servers = McpServers::with_plugin_snapshot(fixture.snapshot(vec![plugin]));

    assert!(matches!(
        servers.plugin_state("invalid-only"),
        PluginRuntimeState::Degraded { servers: 0, warning, .. }
            if warning.contains("mcpServers.broken") && warning.contains("non-empty")
    ));
    assert!(servers
        .reload()
        .await
        .iter()
        .any(|line| line.contains("Plugin invalid-only warning · mcpServers.broken")));
}

#[tokio::test]
async fn unsupported_only_plugin_reports_degraded_mcp_component() {
    let fixture = Fixture::new("unsupported-only");
    let plugin = fixture.plugin("unsupported-only", &[]);
    fixture.replace_mcp(
        &plugin,
        r#""remote":{"type":"streamable-http","url":"https://example.com/mcp"}"#,
    );
    let servers = McpServers::with_plugin_snapshot(fixture.snapshot(vec![plugin]));

    assert!(matches!(
        servers.plugin_state("unsupported-only"),
        PluginRuntimeState::Degraded { servers: 0, warning, .. }
            if warning.contains("unsupported by Orcacode v1")
    ));
    assert!(servers
        .reload()
        .await
        .iter()
        .any(|line| { line.contains("Plugin unsupported-only warning · mcpServers.remote") }));
}

#[tokio::test]
async fn valid_server_with_invalid_sibling_reports_degraded_live_state() {
    let fixture = Fixture::new("mixed-mcp");
    let plugin = fixture.plugin("mixed-mcp", &["echo"]);
    fixture.replace_mcp(
        &plugin,
        r#""echo":{"type":"stdio","command":"sh","args":["${PLUGIN_ROOT}/server.sh","${PLUGIN_DATA}/launch marker"],"env":{"LAUNCH_VALUE":"structured value"},"cwd":"${PLUGIN_ROOT}"},"broken":{"type":"stdio","command":""}"#,
    );
    let servers = McpServers::with_plugin_snapshot(fixture.snapshot(vec![plugin]));

    servers.reload().await;
    assert!(matches!(
        servers.plugin_state("mixed-mcp"),
        PluginRuntimeState::Degraded {
            connected: 1,
            servers: 1,
            tools: 1,
            warning,
        } if warning.contains("mcpServers.broken")
    ));
}

#[tokio::test]
async fn plugin_mcp_inventory_exposes_provenance_and_live_server_state() {
    let fixture = Fixture::new("mcp-inventory");
    let plugin = fixture.plugin("release-tools", &["notes"]);
    let servers = McpServers::with_plugin_snapshot(fixture.snapshot(vec![plugin]));

    let entries = servers.plugin_mcp_entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].label(), "release-tools/notes");
    assert_eq!(entries[0].id, "plugin__release_tools__notes");
    assert_eq!(servers.plugin_mcp_state(&entries[0]), None);

    servers.reload().await;
    assert_eq!(
        servers.plugin_mcp_state(&entries[0]),
        Some(McpState::Connected(1))
    );
}

#[tokio::test]
async fn plugin_data_creation_failure_is_scoped_and_retained() {
    let fixture = Fixture::new("data-failure");
    let plugin = fixture.plugin("data-failure", &["echo"]);
    let blocked_parent = fixture.root.join("blocked-parent");
    let snapshot =
        PluginSnapshot::from_registrations(vec![plugin], |name| Ok(blocked_parent.join(name)));
    fs::write(&blocked_parent, "not a directory").unwrap();
    let servers = McpServers::with_plugin_snapshot(snapshot);

    let lines = servers.reload().await;
    assert!(
        lines.iter().any(|line| {
            line.starts_with("Plugin data-failure MCP echo · cannot create plugin data directory:")
        }),
        "lines: {lines:?}"
    );
    assert!(servers.reload().await.is_empty());
    assert!(matches!(
        servers.state("plugin__data_failure__echo"),
        Some(McpState::Failed(error)) if error.starts_with("cannot create plugin data directory:")
    ));
    assert!(matches!(
        servers.plugin_state("data-failure"),
        PluginRuntimeState::Failed {
            connected: 0,
            servers: 1,
            tools: 0,
            error,
        } if error.starts_with("echo: cannot create plugin data directory:")
    ));
}

#[tokio::test]
async fn desired_id_collisions_exclude_all_affected_plugins_only() {
    let fixture = Fixture::new("collisions");
    let first = fixture.plugin("alpha-plugin", &["echo", "safe"]);
    let second = fixture.plugin("alpha.plugin", &["echo"]);
    let standalone_collision = fixture.plugin("standalone-victim", &["echo"]);
    crate::config::save_mcp_server("plugin__standalone_victim__echo", "orca-no-such-binary-xyz")
        .unwrap();
    let servers = McpServers::with_plugin_snapshot(fixture.snapshot(vec![
        second,
        standalone_collision,
        first,
    ]));

    let lines = servers.reload().await;
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("desired server ID collision"))
            .count(),
        3,
        "lines: {lines:?}"
    );
    let names = tool_names(&servers);
    assert!(names.iter().any(|name| name.contains("alpha_plugin__safe")));
    assert!(!names.iter().any(|name| name.contains("alpha_plugin__echo")));
    assert!(!names.iter().any(|name| name.contains("standalone_victim")));
    assert!(!fixture.data_root.join("alpha.plugin").exists());
    assert!(fixture.data_root.join("alpha-plugin").is_dir());
    assert!(!fixture.data_root.join("standalone-victim").exists());
    let collided = servers
        .plugin_mcp_entries()
        .into_iter()
        .filter(|entry| entry.server == "echo")
        .collect::<Vec<_>>();
    assert_eq!(collided.len(), 3);
    assert!(collided.iter().all(|entry| matches!(
        servers.plugin_mcp_state(entry),
        Some(McpState::Failed(error)) if error == "server ID collision"
    )));
}

#[tokio::test]
async fn changed_standalone_remains_first_in_actual_catalog_order() {
    let fixture = Fixture::new("catalog-reorder");
    let plugin = fixture.plugin("catalog-plugin", &["echo"]);
    let first = fixture.standalone_server("standalone-first");
    crate::config::save_mcp_server("standalone", &format!("sh {}", first.display())).unwrap();
    let servers = McpServers::with_plugin_snapshot(fixture.snapshot(vec![plugin]));
    servers.reload().await;

    let replacement = fixture.standalone_server("standalone-replacement");
    crate::config::save_mcp_server("standalone", &format!("sh {}", replacement.display())).unwrap();
    let lines = servers.reload().await;
    assert!(lines
        .iter()
        .any(|line| line == "MCP standalone disconnected"));
    assert!(lines
        .iter()
        .any(|line| line.starts_with("MCP standalone connected")));
    assert!(!lines.iter().any(|line| line.contains("catalog-plugin MCP")));

    let search = servers
        .tools()
        .into_iter()
        .find(|tool| tool.schema().name == "mcp_search_tools")
        .unwrap();
    let result = search
        .call(
            json!({"query": "structured echo"}),
            &context("mcp_search_tools"),
        )
        .await
        .unwrap();
    assert_eq!(
        result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["server"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["standalone", "plugin__catalog_plugin__echo"]
    );
}

#[test]
fn structured_launch_identity_includes_args_env_cwd_and_environment_policy() {
    let base = StdioLaunch {
        command: "node".into(),
        args: vec!["server.js".into()],
        env: BTreeMap::from([("TOKEN".into(), "one".into())]),
        cwd: Some(Path::new("/plugin").into()),
        environment: ProcessEnvironment::Sanitized,
    };
    let identity = LaunchIdentity::Structured(base.clone());
    for changed in [
        StdioLaunch {
            args: vec!["other.js".into()],
            ..base.clone()
        },
        StdioLaunch {
            env: BTreeMap::from([("TOKEN".into(), "two".into())]),
            ..base.clone()
        },
        StdioLaunch {
            cwd: Some(Path::new("/other").into()),
            ..base.clone()
        },
        StdioLaunch {
            environment: ProcessEnvironment::Inherit,
            ..base.clone()
        },
    ] {
        assert_ne!(identity, LaunchIdentity::Structured(changed));
    }
}
