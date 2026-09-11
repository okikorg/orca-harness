//! The run-boundary refresh of MCP tools and the live server status: a
//! run captures the MCP tool set when it starts and keeps it, registry
//! changes reach the next run of a session, and `Mcp::servers` reads the
//! catalog rather than a copy of it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use orca_harness_core::{FnTool, Message};
use orca_harness_sdk::{Harness, Mcp, McpServerStatus, ToolPreset};
use serde_json::{json, Value};

mod common;
mod schema_support;
use common::temp_dir;
use schema_support::{offers, tool_results, Recording, Step};

const INTERFACE_TOOLS: [&str; 3] = ["mcp_search_tools", "mcp_select_tool", "mcp_features"];

/// A mock stdio MCP server that answers the handshake, lists `tools`, and
/// answers every `tools/call` with one text item, `reply`. It loops on
/// its input so a connection can serve any number of calls.
fn mock_server(root: &Path, label: &str, reply: &str, tools: &[&str]) -> String {
    let listed = tools
        .iter()
        .map(|tool| {
            // Quotes are escaped for the script's double-quoted printf argument.
            format!(
                r#"{{\"name\":\"{tool}\",\"description\":\"{tool} tool\",\"inputSchema\":{{\"type\":\"object\"}}}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{\"tools\":{{}}}},\"serverInfo\":{{\"name\":\"mock\",\"version\":\"1.0.0\"}}}}}}" ;;
    *'"method":"tools/list"'*)
      printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"tools\":[{listed}]}}}}" ;;
    *'"method":"tools/call"'*)
      printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{reply}\"}}]}}}}" ;;
    *)
      # Unexpected requests fail fast instead of hanging the caller;
      # notifications (no id) are dropped.
      [ -n "$id" ] && printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{{\"code\":-32601,\"message\":\"method not found\"}}}}" ;;
  esac
done
"#
    );
    let path: PathBuf = root.join(format!("mock_{label}.sh"));
    std::fs::write(&path, script).unwrap();
    format!("sh {}", path.display())
}

/// The tool results a run added to the session transcript (which
/// `RunResult::messages` carries in full), as `(tool name, is_error,
/// output)`. `seen` counts the results of earlier runs and is advanced.
fn new_results(messages: &[Message], seen: &mut usize) -> Vec<(String, bool, Value)> {
    let all = tool_results(messages);
    let fresh = all[*seen..].to_vec();
    *seen = all.len();
    fresh
}

fn is_unknown_tool(result: &(String, bool, Value)) -> bool {
    result.1
        && result.2["error"]
            .as_str()
            .is_some_and(|error| error.starts_with("unknown tool:"))
}

fn offers_interfaces(schemas: &[Value]) -> bool {
    INTERFACE_TOOLS.iter().all(|tool| offers(schemas, tool))
}

fn select_then_ping() -> Vec<Step> {
    vec![
        Step::Call("mcp_select_tool", json!({"name": "mcp__mock__ping"})),
        Step::Call("mcp__mock__ping", json!({})),
        Step::Final,
    ]
}

fn status(name: &str, tool_count: usize) -> McpServerStatus {
    McpServerStatus {
        name: name.into(),
        healthy: true,
        tool_count,
    }
}

#[tokio::test]
async fn new_connection_is_callable_next_turn_not_mid_run() {
    let root = temp_dir("mcp-next-turn");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let mcp = harness.mcp();
    let model = Arc::new(Recording::default());
    let agent = harness
        .agent(model.clone())
        .tools(ToolPreset::None)
        .mcp(mcp.clone())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();

    let mut seen = 0;
    model.script(vec![Step::Call("mcp__mock__ping", json!({})), Step::Final]);
    let first = session.run("one").await.unwrap();
    let results = new_results(&first.messages, &mut seen);
    assert_eq!(results.len(), 1);
    assert!(is_unknown_tool(&results[0]), "{:?}", results[0]);
    let offered = model.take();
    assert!(offered.iter().all(|schemas| offers_interfaces(schemas)));
    assert!(!offered
        .iter()
        .any(|schemas| offers(schemas, "mcp__mock__ping")));

    let command = mock_server(&root, "pong", "pong", &["ping"]);
    mcp.connect("mock", &command).await.unwrap();

    model.script(select_then_ping());
    let second = session.run("two").await.unwrap();
    let results = new_results(&second.messages, &mut seen);
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(!results[0].1, "{:?}", results[0]);
    assert_eq!(results[1].0, "mcp__mock__ping");
    assert!(!results[1].1, "{:?}", results[1]);
    assert_eq!(results[1].2, json!({"content": "pong"}));
    let offered = model.take();
    assert!(offered.iter().all(|schemas| offers_interfaces(schemas)));
    assert!(
        !offers(&offered[0], "mcp__mock__ping"),
        "remote schemas stay hidden until selected"
    );

    drop(session);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn replaced_connection_serves_the_new_target_next_turn_and_status_is_live() {
    let root = temp_dir("mcp-replace");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let mcp = harness.mcp();
    mcp.connect("mock", &mock_server(&root, "v1", "v1", &["ping"]))
        .await
        .unwrap();
    let model = Arc::new(Recording::default());
    let agent = harness
        .agent(model.clone())
        .tools(ToolPreset::None)
        .mcp(mcp.clone())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();

    let mut seen = 0;
    model.script(select_then_ping());
    let first = session.run("one").await.unwrap();
    let results = new_results(&first.messages, &mut seen);
    assert_eq!(results[1].2, json!({"content": "v1"}), "{results:?}");

    mcp.connect("mock", &mock_server(&root, "v2", "v2", &["ping"]))
        .await
        .unwrap();
    assert_eq!(mcp.servers(), vec![status("mock", 1)]);

    model.script(select_then_ping());
    let second = session.run("two").await.unwrap();
    let results = new_results(&second.messages, &mut seen);
    assert_eq!(results[1].2, json!({"content": "v2"}), "{results:?}");

    mcp.connect("other", &mock_server(&root, "other", "other", &["ping"]))
        .await
        .unwrap();
    assert_eq!(
        mcp.servers(),
        vec![status("mock", 1), status("other", 1)],
        "catalog order"
    );
    assert!(mcp.disconnect("mock"));
    assert_eq!(mcp.servers(), vec![status("other", 1)]);
    assert!(!mcp.disconnect("mock"));

    drop(session);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn disconnect_during_a_run_keeps_that_runs_tools_and_drops_them_next_run() {
    let root = temp_dir("mcp-mid-run");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let mcp = harness.mcp();
    mcp.connect("mock", &mock_server(&root, "pong", "pong", &["ping"]))
        .await
        .unwrap();
    let model = Arc::new(Recording::default());
    let registry = mcp.clone();
    let cut = FnTool::new(
        "cut",
        "disconnects the mock server mid-run",
        json!({"type": "object", "properties": {}}),
        move |_, _| {
            let registry = registry.clone();
            async move { Ok(json!({"disconnected": registry.disconnect("mock")})) }
        },
    );
    let agent = harness
        .agent(model.clone())
        .tools(ToolPreset::None)
        .tool(cut)
        .mcp(mcp.clone())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();

    let mut seen = 0;
    model.script(vec![
        Step::Call("mcp_select_tool", json!({"name": "mcp__mock__ping"})),
        Step::Call("mcp__mock__ping", json!({})),
        Step::Call("cut", json!({})),
        Step::Call("mcp__mock__ping", json!({})),
        Step::Final,
    ]);
    let first = session.run("one").await.unwrap();
    let results = new_results(&first.messages, &mut seen);
    assert_eq!(results.len(), 4, "{results:?}");
    assert_eq!(results[1].2, json!({"content": "pong"}));
    assert_eq!(results[2].2, json!({"disconnected": true}));
    assert_eq!(results[3].0, "mcp__mock__ping");
    assert!(
        !results[3].1,
        "the run keeps its captured tool: {:?}",
        results[3]
    );
    assert_eq!(results[3].2, json!({"content": "pong"}));
    assert!(mcp.servers().is_empty());

    model.script(vec![Step::Call("mcp__mock__ping", json!({})), Step::Final]);
    let second = session.run("two").await.unwrap();
    let results = new_results(&second.messages, &mut seen);
    assert_eq!(results.len(), 1);
    assert!(is_unknown_tool(&results[0]), "{:?}", results[0]);

    drop(session);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn mcp_status_and_tools_have_one_source() {
    let root = temp_dir("mcp-one-source");
    let mcp = Mcp::new();
    let remote_tools = |mcp: &Mcp| {
        mcp.tools()
            .iter()
            .filter(|tool| tool.schema().name.starts_with("mcp__mock__"))
            .count()
    };

    mcp.connect("mock", &mock_server(&root, "one", "pong", &["ping"]))
        .await
        .unwrap();
    assert_eq!(mcp.servers(), vec![status("mock", 1)]);
    assert_eq!(remote_tools(&mcp), 1);

    mcp.connect(
        "mock",
        &mock_server(&root, "two", "pong", &["ping", "echo"]),
    )
    .await
    .unwrap();
    assert_eq!(mcp.servers(), vec![status("mock", 2)]);
    assert_eq!(remote_tools(&mcp), 2);

    let _ = std::fs::remove_dir_all(&root);
}
