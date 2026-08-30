use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use orca_harness_tool_extensions::agent_plugins::{
    load_agent_plugin, normalize_plugin_server_id, AgentPlugin,
};
use orca_harness_tool_extensions::mcp::ProcessEnvironment;
use serde_json::{json, Value};

#[path = "agent_plugin/review_regressions.rs"]
mod review_regressions;

const PLUGIN_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
const MCP_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json";

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TestTree {
    path: PathBuf,
}

impl TestTree {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "orca-agent-plugin-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn plugin(&self) -> PathBuf {
        let path = self.path.join("plugin");
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_json(&self, path: impl AsRef<Path>, value: Value) {
        fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }
}

impl Drop for TestTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn manifest(name: &str) -> Value {
    json!({ "$schema": PLUGIN_SCHEMA, "name": name })
}

fn write_manifest(tree: &TestTree, root: &Path, value: Value) {
    tree.write_json(root.join("plugin.json"), value);
}

fn write_mcp(tree: &TestTree, root: &Path, servers: Value) {
    tree.write_json(
        root.join("mcp.json"),
        json!({ "$schema": MCP_SCHEMA, "mcpServers": servers }),
    );
}

fn warning_text(plugin: &AgentPlugin) -> String {
    plugin
        .warnings
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn agent_plugin_loads_minimal_manifest_without_creating_plugin_data() {
    let tree = TestTree::new("minimal");
    let root = tree.plugin();
    let data = tree.path.join("state/not-created");
    write_manifest(&tree, &root, manifest("minimal-plugin"));

    let plugin = load_agent_plugin(&root, &data).unwrap();

    assert_eq!(plugin.name, "minimal-plugin");
    assert_eq!(plugin.version, None);
    assert_eq!(plugin.root, fs::canonicalize(&root).unwrap());
    assert!(plugin.mcp_servers.is_empty());
    assert!(plugin.warnings.is_empty());
    assert!(
        !data.exists(),
        "static validation must not create plugin data"
    );
}

#[test]
fn agent_plugin_builds_sanitized_structured_stdio_launch() {
    let tree = TestTree::new("stdio");
    let root = tree.plugin();
    let data = tree.path.join("state/plugin-data");
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("bin/server"), "binary fixture").unwrap();
    write_manifest(
        &tree,
        &root,
        json!({ "$schema": PLUGIN_SCHEMA, "name": "tools.plugin", "version": "1.2.3" }),
    );
    write_mcp(
        &tree,
        &root,
        json!({
            "main server": {
                "type": "stdio",
                "command": "./bin/server",
                "args": ["--root=${PLUGIN_ROOT}", "${PLUGIN_DATA}/cache", "${OTHER}"],
                "env": { "CONFIG": "${PLUGIN_ROOT}/config.json", "MIXED": "a${PLUGIN_DATA}b" },
                "cwd": "${PLUGIN_DATA}/work"
            }
        }),
    );

    let plugin = load_agent_plugin(&root, &data).unwrap();
    let server = &plugin.mcp_servers[0];
    let canonical_root = fs::canonicalize(&root).unwrap();
    let normalized_data = fs::canonicalize(&tree.path)
        .unwrap()
        .join("state/plugin-data");

    assert_eq!(server.id, "plugin__tools_plugin__main_server");
    assert_eq!(server.plugin_name, "tools.plugin");
    assert_eq!(server.server_name, "main server");
    assert_eq!(
        server.launch.command,
        canonical_root.join("bin/server").to_string_lossy()
    );
    assert_eq!(
        server.launch.args,
        vec![
            format!("--root={}", canonical_root.display()),
            format!("{}/cache", normalized_data.display()),
            "${OTHER}".to_string(),
        ]
    );
    assert_eq!(
        server.launch.env["CONFIG"],
        format!("{}/config.json", canonical_root.display())
    );
    assert_eq!(
        server.launch.env["MIXED"],
        format!("a{}b", normalized_data.display())
    );
    assert_eq!(
        server.launch.env["PLUGIN_ROOT"],
        canonical_root.to_string_lossy()
    );
    assert_eq!(
        server.launch.env["PLUGIN_DATA"],
        normalized_data.to_string_lossy()
    );
    assert_eq!(server.launch.cwd, Some(normalized_data.join("work")));
    assert_eq!(server.launch.environment, ProcessEnvironment::Sanitized);
}

#[test]
fn agent_plugin_rejects_unsupported_or_missing_manifest_schema() {
    for (label, value) in [
        ("missing", json!({ "name": "valid-name" })),
        (
            "unsupported",
            json!({ "$schema": "https://agent-plugins.org/schemas/2.0.0/plugin.schema.json", "name": "valid-name" }),
        ),
    ] {
        let tree = TestTree::new(label);
        let root = tree.plugin();
        write_manifest(&tree, &root, value);
        let error = load_agent_plugin(&root, &tree.path.join("data")).unwrap_err();
        assert!(error.to_string().contains("$schema"), "{error}");
    }
}

#[test]
fn agent_plugin_enforces_names_and_standard_manifest_types() {
    let invalid_names = ["", "A", "-start", "end.", "has--gap", "has..gap"];
    for name in invalid_names {
        let tree = TestTree::new("name");
        let root = tree.plugin();
        write_manifest(&tree, &root, manifest(name));
        let error = load_agent_plugin(&root, &tree.path.join("data")).unwrap_err();
        assert!(error.to_string().contains("name"), "{name:?}: {error}");
    }

    let tree = TestTree::new("metadata");
    let root = tree.plugin();
    write_manifest(
        &tree,
        &root,
        json!({ "$schema": PLUGIN_SCHEMA, "name": "valid", "author": { "handle": "nope" } }),
    );
    let error = load_agent_plugin(&root, &tree.path.join("data")).unwrap_err();
    assert!(error.to_string().contains("author"), "{error}");
}

#[test]
fn agent_plugin_warns_for_ignored_manifest_fields_skills_and_extensions() {
    let tree = TestTree::new("warnings");
    let root = tree.plugin();
    fs::create_dir(root.join("skills")).unwrap();
    write_manifest(
        &tree,
        &root,
        json!({
            "$schema": PLUGIN_SCHEMA,
            "name": "warning-plugin",
            "unknown": true,
            "extensions": { "com.example.client": 42 }
        }),
    );

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();
    let warnings = warning_text(&plugin);

    assert!(warnings.contains("unknown"), "{warnings}");
    assert!(
        warnings.contains("extensions.com.example.client"),
        "{warnings}"
    );
    assert!(warnings.contains("skills"), "{warnings}");

    write_manifest(
        &tree,
        &root,
        json!({ "$schema": PLUGIN_SCHEMA, "name": "warning-plugin", "extensions": [] }),
    );
    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();
    assert!(warning_text(&plugin).contains("extensions"));
}

#[test]
fn invalid_mcp_document_warns_and_disables_only_mcp() {
    let tree = TestTree::new("invalid-mcp");
    let root = tree.plugin();
    write_manifest(&tree, &root, manifest("valid-plugin"));
    tree.write_json(
        root.join("mcp.json"),
        json!({ "$schema": MCP_SCHEMA, "mcpServers": {}, "extra": true }),
    );

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();

    assert!(plugin.mcp_servers.is_empty());
    assert!(warning_text(&plugin).contains("mcp.json"));

    for (label, document) in [
        ("missing-schema", json!({ "mcpServers": {} })),
        (
            "unsupported-schema",
            json!({ "$schema": "https://agent-plugins.org/schemas/2.0.0/mcp.schema.json", "mcpServers": {} }),
        ),
    ] {
        let tree = TestTree::new(label);
        let root = tree.plugin();
        write_manifest(&tree, &root, manifest("valid-plugin"));
        tree.write_json(root.join("mcp.json"), document);
        let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();
        assert!(plugin.mcp_servers.is_empty());
        assert!(warning_text(&plugin).contains("mcp.json"));
    }
}

#[test]
fn invalid_and_unsupported_servers_preserve_valid_stdio_siblings() {
    let tree = TestTree::new("server-isolation");
    let root = tree.plugin();
    write_manifest(&tree, &root, manifest("server-plugin"));
    write_mcp(
        &tree,
        &root,
        json!({
            "good": { "type": "stdio", "command": "node" },
            "bad": { "type": "stdio", "command": "node", "extra": true },
            "remote": { "type": "streamable-http", "url": "https://example.com/mcp" },
            "legacy": { "type": "sse", "url": "https://example.com/sse" }
        }),
    );

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();

    assert_eq!(plugin.mcp_servers.len(), 1);
    assert_eq!(plugin.mcp_servers[0].server_name, "good");
    let warnings = warning_text(&plugin);
    assert!(warnings.contains("mcpServers.bad"), "{warnings}");
    assert!(warnings.contains("mcpServers.remote"), "{warnings}");
    assert!(warnings.contains("mcpServers.legacy"), "{warnings}");
}

#[test]
fn invalid_remote_server_semantics_are_not_reported_as_supported_configuration() {
    let tree = TestTree::new("remote-validation");
    let root = tree.plugin();
    write_manifest(&tree, &root, manifest("remote-plugin"));
    write_mcp(
        &tree,
        &root,
        json!({
            "insecure": { "type": "streamable-http", "url": "http://example.com/mcp" },
            "fragment": { "type": "sse", "url": "https://example.com/sse#events" },
            "duplicate-header": {
                "type": "streamable-http",
                "url": "https://example.com/mcp",
                "headers": { "X-Test": "one", "x-test": "two" }
            }
        }),
    );

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();
    let warnings = warning_text(&plugin);

    assert!(
        warnings.contains("mcpServers.insecure") && warnings.contains("HTTPS"),
        "{warnings}"
    );
    assert!(
        warnings.contains("mcpServers.fragment") && warnings.contains("fragment"),
        "{warnings}"
    );
    assert!(
        warnings.contains("mcpServers.duplicate-header") && warnings.contains("header"),
        "{warnings}"
    );
}

#[test]
fn command_is_a_direct_unexpanded_executable_token() {
    let tree = TestTree::new("command-token");
    let root = tree.plugin();
    write_manifest(&tree, &root, manifest("command-plugin"));
    write_mcp(
        &tree,
        &root,
        json!({
            "bare": { "type": "stdio", "command": "${PLUGIN_ROOT}" },
            "shell": { "type": "stdio", "command": "node server.js" },
            "absolute": { "type": "stdio", "command": "/bin/sh" }
        }),
    );

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();

    assert_eq!(plugin.mcp_servers.len(), 1);
    assert_eq!(plugin.mcp_servers[0].launch.command, "${PLUGIN_ROOT}");
    let warnings = warning_text(&plugin);
    assert!(warnings.contains("mcpServers.shell"), "{warnings}");
    assert!(warnings.contains("mcpServers.absolute"), "{warnings}");
}

#[test]
fn reserved_environment_keys_invalidate_only_their_server() {
    let tree = TestTree::new("reserved-env");
    let root = tree.plugin();
    write_manifest(&tree, &root, manifest("env-plugin"));
    write_mcp(
        &tree,
        &root,
        json!({
            "reserved": { "type": "stdio", "command": "node", "env": { "PLUGIN_ROOT": "wrong" } },
            "good": { "type": "stdio", "command": "node", "env": { "HOME_OVERRIDE": "ok" } }
        }),
    );

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();

    assert_eq!(plugin.mcp_servers.len(), 1);
    assert_eq!(plugin.mcp_servers[0].server_name, "good");
    assert!(warning_text(&plugin).contains("mcpServers.reserved"));
}

#[test]
fn lexical_path_escapes_in_command_and_cwd_are_isolated() {
    let tree = TestTree::new("lexical-escape");
    let root = tree.plugin();
    write_manifest(&tree, &root, manifest("path-plugin"));
    write_mcp(
        &tree,
        &root,
        json!({
            "command-escape": { "type": "stdio", "command": "./../outside" },
            "root-cwd-escape": { "type": "stdio", "command": "node", "cwd": "${PLUGIN_ROOT}/../outside" },
            "data-cwd-escape": { "type": "stdio", "command": "node", "cwd": "${PLUGIN_DATA}/../outside" },
            "good": { "type": "stdio", "command": "node", "cwd": "./work" }
        }),
    );

    let plugin = load_agent_plugin(&root, &tree.path.join("data/plugin")).unwrap();

    assert_eq!(plugin.mcp_servers.len(), 1);
    assert_eq!(plugin.mcp_servers[0].server_name, "good");
    assert_eq!(
        plugin.mcp_servers[0].launch.cwd,
        Some(fs::canonicalize(&root).unwrap().join("work"))
    );
}

#[cfg(unix)]
#[test]
fn symlink_escapes_in_package_paths_commands_and_cwd_are_rejected() {
    use std::os::unix::fs::symlink;

    let tree = TestTree::new("symlink-escape");
    let root = tree.plugin();
    let outside = tree.path.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("server"), "outside").unwrap();
    symlink(&outside, root.join("escape")).unwrap();
    write_manifest(&tree, &root, manifest("symlink-plugin"));
    write_mcp(
        &tree,
        &root,
        json!({
            "command": { "type": "stdio", "command": "./escape/server" },
            "cwd": { "type": "stdio", "command": "node", "cwd": "./escape" },
            "good": { "type": "stdio", "command": "node" }
        }),
    );

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();
    assert_eq!(plugin.mcp_servers.len(), 1);
    assert_eq!(plugin.mcp_servers[0].server_name, "good");

    fs::remove_file(root.join("mcp.json")).unwrap();
    tree.write_json(
        outside.join("mcp.json"),
        json!({ "$schema": MCP_SCHEMA, "mcpServers": {} }),
    );
    symlink(outside.join("mcp.json"), root.join("mcp.json")).unwrap();
    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();
    assert!(plugin.mcp_servers.is_empty());
    assert!(warning_text(&plugin).contains("mcp.json"));

    fs::remove_file(root.join("plugin.json")).unwrap();
    symlink(outside.join("server"), root.join("plugin.json")).unwrap();
    let error = load_agent_plugin(&root, &tree.path.join("data")).unwrap_err();
    assert!(error.to_string().contains("plugin.json"), "{error}");
}

#[cfg(unix)]
#[test]
fn existing_plugin_data_symlink_escape_invalidates_cwd() {
    use std::os::unix::fs::symlink;

    let tree = TestTree::new("data-symlink");
    let root = tree.plugin();
    let data = tree.path.join("data");
    let outside = tree.path.join("outside");
    fs::create_dir(&data).unwrap();
    fs::create_dir(&outside).unwrap();
    symlink(&outside, data.join("escape")).unwrap();
    write_manifest(&tree, &root, manifest("data-plugin"));
    write_mcp(
        &tree,
        &root,
        json!({
            "escaped": { "type": "stdio", "command": "node", "cwd": "${PLUGIN_DATA}/escape" },
            "good": { "type": "stdio", "command": "node", "cwd": "${PLUGIN_DATA}/new/work" }
        }),
    );

    let plugin = load_agent_plugin(&root, &data).unwrap();

    assert_eq!(plugin.mcp_servers.len(), 1);
    assert_eq!(plugin.mcp_servers[0].server_name, "good");
    assert_eq!(
        plugin.mcp_servers[0].launch.cwd,
        Some(fs::canonicalize(&data).unwrap().join("new/work"))
    );
}

#[cfg(unix)]
#[test]
fn broken_symlinks_in_plugin_data_descendants_are_rejected() {
    use std::os::unix::fs::symlink;

    let tree = TestTree::new("broken-data-symlink");
    let root = tree.plugin();
    let data = tree.path.join("data");
    fs::create_dir(&data).unwrap();
    symlink(tree.path.join("missing-target"), data.join("broken")).unwrap();
    write_manifest(&tree, &root, manifest("broken-data-plugin"));
    write_mcp(
        &tree,
        &root,
        json!({
            "broken": { "type": "stdio", "command": "node", "cwd": "${PLUGIN_DATA}/broken/work" },
            "good": { "type": "stdio", "command": "node" }
        }),
    );

    let plugin = load_agent_plugin(&root, &data).unwrap();

    assert_eq!(plugin.mcp_servers.len(), 1);
    assert_eq!(plugin.mcp_servers[0].server_name, "good");
    assert!(warning_text(&plugin).contains("mcpServers.broken"));
}

#[test]
fn placeholder_expansion_is_single_pass() {
    let tree = TestTree::new("single-pass");
    let root = tree.path.join("${PLUGIN_DATA}");
    fs::create_dir(&root).unwrap();
    write_manifest(&tree, &root, manifest("expansion-plugin"));
    write_mcp(
        &tree,
        &root,
        json!({
            "server": {
                "type": "stdio",
                "command": "node",
                "args": ["${PLUGIN_ROOT}", "${PLUGIN_DATA}", "${PLUGIN_ROOTISH}"]
            }
        }),
    );
    let data = tree.path.join("data");

    let plugin = load_agent_plugin(&root, &data).unwrap();

    assert_eq!(
        plugin.mcp_servers[0].launch.args[0],
        fs::canonicalize(&root).unwrap().to_string_lossy()
    );
    assert_eq!(
        plugin.mcp_servers[0].launch.args[1],
        fs::canonicalize(&tree.path)
            .unwrap()
            .join("data")
            .to_string_lossy()
    );
    assert_eq!(plugin.mcp_servers[0].launch.args[2], "${PLUGIN_ROOTISH}");
}

#[test]
fn normalized_ids_are_deterministic_and_collisions_remove_all_affected_servers() {
    assert_eq!(
        normalize_plugin_server_id("My.Plugin", "Build Server!"),
        "plugin__my_plugin__build_server"
    );
    assert_eq!(
        normalize_plugin_server_id("My.Plugin", "Build---Server"),
        "plugin__my_plugin__build_server"
    );

    let tree = TestTree::new("id-collision");
    let root = tree.plugin();
    write_manifest(&tree, &root, manifest("collision.plugin"));
    write_mcp(
        &tree,
        &root,
        json!({
            "a-b": { "type": "stdio", "command": "node" },
            "a.b": { "type": "stdio", "command": "node" },
            "safe": { "type": "stdio", "command": "node" }
        }),
    );

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();

    assert_eq!(plugin.mcp_servers.len(), 1);
    assert_eq!(plugin.mcp_servers[0].server_name, "safe");
    assert!(warning_text(&plugin).contains("collision"));
}

#[test]
fn root_and_plugin_data_boundaries_must_be_absolute_and_well_formed() {
    let tree = TestTree::new("boundaries");
    let root = tree.plugin();
    write_manifest(&tree, &root, manifest("boundary-plugin"));

    let relative_data = Path::new("relative/plugin-data");
    let error = load_agent_plugin(&root, relative_data).unwrap_err();
    assert!(error.to_string().contains("plugin data"), "{error}");

    let error =
        load_agent_plugin(Path::new("relative/plugin"), &tree.path.join("data")).unwrap_err();
    assert!(error.to_string().contains("plugin root"), "{error}");
}
