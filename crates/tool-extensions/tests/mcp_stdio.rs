//! End-to-end over a real child process: a canned `sh` MCP server that
//! answers the client's fixed handshake ids (1: initialize, 2:
//! tools/list, 3: the first tools/call).

#![cfg(unix)]

use orca_harness_core::{CancellationToken, ToolContext};
use orca_harness_tool_extensions::mcp::{McpClient, McpError};
use serde_json::json;

const FAKE_SERVER: &str = r#"#!/bin/sh
read _initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0.0.0"}}}'
read _initialized
read _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echo back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}'
read _call
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"hello back"}]}}'
"#;

fn ctx(tool_name: &str) -> ToolContext {
    ToolContext {
        call_id: "call-1".into(),
        tool_name: tool_name.into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

#[tokio::test]
async fn connects_lists_and_calls_through_a_stdio_server() {
    let dir = std::env::temp_dir().join(format!("orca-mcp-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("fake-mcp.sh");
    std::fs::write(&script, FAKE_SERVER).unwrap();

    let tools = McpClient::connect("fake", &format!("sh {}", script.display()))
        .await
        .unwrap();
    assert_eq!(tools.len(), 1);

    let schema = tools[0].schema();
    assert_eq!(schema.name, "mcp__fake__echo");
    assert_eq!(schema.description, "echo back");
    assert_eq!(schema.parameters["type"], "object");

    let output = tools[0]
        .call(json!({ "text": "hi" }), &ctx(&schema.name))
        .await
        .unwrap();
    assert_eq!(output, json!({ "content": "hello back" }));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_command_that_exits_immediately_fails_the_handshake() {
    let Err(err) = McpClient::connect("dead", "true").await else {
        panic!("expected the handshake to fail");
    };
    assert!(
        matches!(err, McpError::Protocol(ref m) if m.contains("closed")),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn empty_and_unspawnable_commands_are_errors() {
    let Err(err) = McpClient::connect("blank", "   ").await else {
        panic!("expected an empty command to be rejected");
    };
    assert!(matches!(err, McpError::Protocol(_)), "got: {err}");

    let Err(err) = McpClient::connect("missing", "orca-no-such-binary-xyz").await else {
        panic!("expected the spawn to fail");
    };
    assert!(matches!(err, McpError::Spawn(_)), "got: {err}");
}
