use orca_harness_core::{Extension, ToolCall, ToolDecision};

use super::*;

#[tokio::test]
async fn enabled_plugin_hooks_prepare_data_and_join_the_extension_lifecycle() {
    let fixture = Fixture::new("hooks");
    let plugin = fixture.plugin_without_mcp("policy-plugin");
    fixture.add_deny_hook(&plugin);
    let data = fixture.data_root.join("policy-plugin");
    let servers = McpServers::with_plugin_snapshot(fixture.snapshot(vec![plugin]));

    assert_eq!(servers.plugin_hook_count("policy-plugin"), 1);
    assert!(!data.exists(), "static snapshot created plugin data");
    let (hooks, notices) = servers.plugin_hook_extension();
    assert!(notices.is_empty(), "{notices:?}");
    assert!(
        data.is_dir(),
        "runtime hook preparation did not create data"
    );
    let hooks = hooks.expect("hook extension");
    let decision = hooks
        .before_tool(&ToolCall {
            id: "call-1".into(),
            name: "write_file".into(),
            arguments: json!({ "path": "blocked" }),
        })
        .await
        .unwrap();
    assert!(matches!(decision, ToolDecision::Deny { reason } if reason == "plugin policy"));
}
