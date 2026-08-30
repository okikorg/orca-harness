use super::*;

fn write_hooks(tree: &TestTree, root: &Path, hooks: Value) {
    tree.write_json(
        root.join("io.github.okikorg.orcacode/hooks.json"),
        json!({ "version": 1, "hooks": hooks }),
    );
}

#[test]
fn loads_orcacode_hooks_with_structured_sanitized_launches() {
    let tree = TestTree::new("hooks");
    let root = tree.plugin();
    let data = tree.path.join("state/plugin-data");
    fs::create_dir_all(root.join("scripts")).unwrap();
    fs::write(root.join("scripts/check"), "fixture").unwrap();
    write_manifest(
        &tree,
        &root,
        json!({
            "$schema": PLUGIN_SCHEMA,
            "name": "hook-plugin",
            "extensions": { "io.github.okikorg.orcacode": {} }
        }),
    );
    write_hooks(
        &tree,
        &root,
        json!({
            "before_tool": [
                {
                    "command": "./scripts/check",
                    "args": ["${PLUGIN_ROOT}", "${PLUGIN_DATA}/audit"],
                    "env": { "AUDIT": "${PLUGIN_DATA}/events.jsonl" },
                    "cwd": "${PLUGIN_ROOT}",
                    "timeout_ms": 1250
                },
                { "command": ["not", "a", "string"] }
            ],
            "model_delta": [{ "command": "ignored" }]
        }),
    );

    let plugin = load_agent_plugin(&root, &data).unwrap();

    assert_eq!(plugin.hooks.len(), 1);
    let hook = &plugin.hooks[0];
    assert_eq!(hook.plugin_name, "hook-plugin");
    assert_eq!(hook.event.as_str(), "before_tool");
    assert_eq!(hook.index, 0);
    assert_eq!(hook.timeout, std::time::Duration::from_millis(1250));
    assert_eq!(hook.launch.environment, ProcessEnvironment::Sanitized);
    assert_eq!(hook.launch.cwd, Some(fs::canonicalize(&root).unwrap()));
    assert_eq!(
        hook.launch.env["PLUGIN_ROOT"],
        fs::canonicalize(&root).unwrap().to_string_lossy()
    );
    assert_eq!(
        hook.launch.env["PLUGIN_DATA"],
        fs::canonicalize(&tree.path)
            .unwrap()
            .join("state/plugin-data")
            .to_string_lossy()
    );
    let warnings = warning_text(&plugin);
    assert!(warnings.contains("before_tool[1]"), "{warnings}");
    assert!(warnings.contains("model_delta"), "{warnings}");
    assert!(
        !warnings.contains("does not load this client extension namespace"),
        "{warnings}"
    );
    assert!(
        !data.exists(),
        "static hook validation must not create data"
    );
}

#[test]
fn invalid_document_does_not_disable_portable_components() {
    let tree = TestTree::new("invalid-hooks");
    let root = tree.plugin();
    write_manifest(&tree, &root, manifest("mixed-plugin"));
    write_mcp(&tree, &root, json!({}));
    tree.write_json(
        root.join("io.github.okikorg.orcacode/hooks.json"),
        json!({ "version": 2, "hooks": {} }),
    );

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();

    assert!(plugin.hooks.is_empty());
    assert!(warning_text(&plugin).contains("unsupported hooks version 2"));
    assert!(plugin.mcp_servers.is_empty());
}
