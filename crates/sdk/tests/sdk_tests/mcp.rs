use super::temp_dir;
use orca_harness_core::testing::ScriptedModel;
use orca_harness_core::ModelResponse;
use orca_harness_sdk::{Harness, McpServerStatus};

#[tokio::test]
async fn mcp_empty_manager_interface_and_status() {
    let root = temp_dir("mcp-empty");
    let harness = Harness::builder().workspace(&root).build().unwrap();

    let mcp = harness.mcp();

    // Status on empty MCP manager
    let servers = mcp.servers();
    assert!(servers.is_empty());

    // MCP catalog should offer interface tools (mcp_search_tools, mcp_select_tool, mcp_features)
    let tools = mcp.tools();
    let tool_names: Vec<String> = tools.iter().map(|t| t.schema().name).collect();
    assert!(tool_names.contains(&"mcp_search_tools".to_string()));
    assert!(tool_names.contains(&"mcp_select_tool".to_string()));
    assert!(tool_names.contains(&"mcp_features".to_string()));

    // Disconnect nonexistent server
    assert!(!mcp.disconnect("nonexistent"));

    // Agent with MCP configured
    let model = ScriptedModel::new(vec![ModelResponse::final_text("mcp ready")]);
    let agent = harness.agent(model).mcp(mcp).build().unwrap();

    let session = agent.new_session().ephemeral().open().unwrap();
    let res = session.run("test mcp").await.unwrap();
    assert_eq!(res.text, "mcp ready");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn mcp_mock_stdio_server_connection() {
    let root = temp_dir("mcp-stdio");
    let harness = Harness::builder().workspace(&root).build().unwrap();

    // Create a local hermetic mock MCP stdio server script
    let server_script = root.join("mock_mcp.sh");
    std::fs::write(
        &server_script,
        r#"#!/bin/sh
read _initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"1.0.0"}}}'
read _initialized
read _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"ping","description":"ping tool","inputSchema":{"type":"object"}}]}}'
"#,
    )
    .unwrap();

    let mcp = harness.mcp();
    let status = mcp
        .connect("mock", &format!("sh {}", server_script.display()))
        .await
        .unwrap();

    assert_eq!(
        status,
        McpServerStatus {
            name: "mock".into(),
            healthy: true,
            tool_count: 1,
        }
    );

    let servers = mcp.servers();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "mock");
    assert!(servers[0].healthy);

    let tools = mcp.tools();
    let tool_names: Vec<String> = tools.iter().map(|t| t.schema().name).collect();
    assert!(tool_names.contains(&"mcp__mock__ping".to_string()));

    // Disconnect
    assert!(mcp.disconnect("mock"));
    assert!(mcp.servers().is_empty());

    let _ = std::fs::remove_dir_all(&root);
}
