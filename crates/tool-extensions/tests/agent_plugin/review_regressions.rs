use super::*;

#[test]
fn agent_plugin_cwd_expands_all_placeholders_and_accepts_repeated_separators() {
    let tree = TestTree::new("cwd-expansion");
    let root = tree.plugin();
    let data = tree.path.join("data");
    write_manifest(&tree, &root, manifest("cwd-plugin"));
    write_mcp(
        &tree,
        &root,
        json!({
            "mixed": {
                "type": "stdio",
                "command": "node",
                "cwd": "${PLUGIN_ROOT}/cache-${PLUGIN_DATA}/work"
            },
            "root-repeated": {
                "type": "stdio",
                "command": "node",
                "cwd": "${PLUGIN_ROOT}//work"
            },
            "relative-repeated": {
                "type": "stdio",
                "command": "node",
                "cwd": ".//work"
            }
        }),
    );

    let plugin = load_agent_plugin(&root, &data).unwrap();
    let canonical_root = fs::canonicalize(&root).unwrap();
    let normalized_data = fs::canonicalize(&tree.path).unwrap().join("data");
    let mixed_expected = PathBuf::from(format!(
        "{}/cache-{}/work",
        canonical_root.display(),
        normalized_data.display()
    ));
    let cwd = |name: &str| {
        plugin
            .mcp_servers
            .iter()
            .find(|server| server.server_name == name)
            .and_then(|server| server.launch.cwd.clone())
    };

    assert_eq!(cwd("mixed"), Some(mixed_expected));
    assert_eq!(cwd("root-repeated"), Some(canonical_root.join("work")));
    assert_eq!(cwd("relative-repeated"), Some(canonical_root.join("work")));
    assert!(plugin.warnings.is_empty(), "{}", warning_text(&plugin));
}

#[cfg(unix)]
#[test]
fn agent_plugin_rejects_non_utf8_root_and_data_boundaries() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let tree = TestTree::new("non-utf8");
    let invalid_root = tree.path.join(OsString::from_vec(b"plugin-\xff".to_vec()));
    let error = load_agent_plugin(&invalid_root, &tree.path.join("data")).unwrap_err();
    assert!(error.to_string().contains("UTF-8"), "{error}");

    let root = tree.plugin();
    write_manifest(&tree, &root, manifest("utf8-plugin"));
    let invalid_data = tree.path.join(OsString::from_vec(b"data-\xff".to_vec()));
    let error = load_agent_plugin(&root, &invalid_data).unwrap_err();
    assert!(error.to_string().contains("UTF-8"), "{error}");
    assert!(!invalid_data.exists());
}
