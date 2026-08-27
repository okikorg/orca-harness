//! Demonstrates a hermetic local MCP flow:
//!   1. Create an MCP manager and inspect empty status + interface tools.
//!   2. Launch a mock stdio MCP server (inline shell script) — connect, check
//!      status, discover tool catalog entries, disconnect.
//!   3. Build an agent that consumes the connected MCP (tool catalog wrapped
//!      into the model), run it with ScriptedModel, no network needed.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{FnTool, ModelResponse, Usage};
use orca_harness_sdk::Harness;
use serde_json::json;

fn temp_dir(_label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "orca-mcp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// 1 · EMPTY MCP MANAGER
// ---------------------------------------------------------------------------

fn empty_mcp(harness: &Harness) {
    println!("=== Empty MCP Manager ===");
    let mcp = harness.mcp();

    // No servers connected yet
    let servers = mcp.servers();
    assert!(servers.is_empty());
    println!("  Connected servers: {}", servers.len());

    // Interface tools are always available
    let tools = mcp.tools();
    let names: Vec<String> = tools.iter().map(|t| t.schema().name.clone()).collect();
    println!("  Catalog tools (interface only): {:?}", names);
    assert!(names.contains(&"mcp_search_tools".into()));
    assert!(names.contains(&"mcp_select_tool".into()));
    assert!(names.contains(&"mcp_features".into()));

    // Disconnecting nonexistent server is safe
    assert!(!mcp.disconnect("ghost"));
    println!("  OK - Empty MCP manager verified\n");
}

// ---------------------------------------------------------------------------
// 2 · MOCK STDIO SERVER
// ---------------------------------------------------------------------------

async fn mock_stdio_server(root: &std::path::Path) {
    println!("=== Mock Stdio MCP Server ===");

    // Write an inline JSON-RPC stdio server to disk inside the temp workspace.
    // It responds to the standard MCP initialization handshake and list-tools.
    let server_script = root.join("mock_mcp.sh");
    std::fs::write(
        &server_script,
        "\
#!/bin/sh\n\
read _initialize\n\
printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"echo-server\",\"version\":\"1.0.0\"}}}'\n\
read _initialized\n\
read _list\n\
printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"echo\",\"description\":\"Echoes back its input string\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"message\":{\"type\":\"string\"}}}},{\"name\":\"add_two\",\"description\":\"Adds two numbers together\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"a\":{\"type\":\"number\"},\"b\":{\"type\":\"number\"}}}}]}}'\n\
",
    )
    .unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&server_script, std::fs::Permissions::from_mode(0o755))
        .expect("chmod");

    let harness = Harness::builder().workspace(root).build().unwrap();
    let mcp = harness.mcp();

    // Connect using `sh` (works cross-platform; avoids PATH issues).
    let cmd = format!("sh {}", server_script.display());
    let status = mcp.connect("echo", &cmd).await.unwrap();
    println!(
        "  Connected: name={} healthy={} tools={}",
        status.name, status.healthy, status.tool_count
    );
    assert_eq!(status.name, "echo");
    assert!(status.healthy);
    assert_eq!(status.tool_count, 2);

    // Status reflects connection
    let alive = mcp.servers();
    assert_eq!(alive.len(), 1);
    assert_eq!(alive[0].name, "echo");
    assert!(alive[0].healthy);
    println!("  servers() -> {:?}", alive[0]);

    // Tool catalog now includes both the MCP interface tools AND the
    // server's tools prefixed as mcp__echo__*
    let all_tools = mcp.tools();
    let names: Vec<String> = all_tools.iter().map(|t| t.schema().name.clone()).collect();
    println!("  All catalog tools: {:?}", names);
    assert!(names.contains(&"mcp__echo__echo".into()));
    assert!(names.contains(&"mcp__echo__add_two".into()));
    assert!(names.contains(&"mcp_search_tools".into()));

    // Disconnect
    assert!(mcp.disconnect("echo"));
    assert!(mcp.servers().is_empty());
    println!("  Disconnected 'echo', servers now empty");

    // Verify reconnect works with a fresh server
    let status2 = mcp.connect("echo", &cmd).await.unwrap();
    assert_eq!(status2.tool_count, 2);
    assert!(mcp.disconnect("echo"));

    println!("  OK - Mock stdio server lifecycle complete\n");
}

// ---------------------------------------------------------------------------
// 3 · AGENT WITH MCP CONFIGURED (no live calls)
// ---------------------------------------------------------------------------

async fn agent_with_mcp(root: &std::path::Path) {
    println!("=== Agent Configured with MCP (no live calls) ===");

    let harness = Harness::builder().workspace(root).build().unwrap();
    let mcp = harness.mcp();
    // Don't connect any servers for this sub-test — just prove the agent
    // builder accepts an MCP and the agent runs fine. The MCP interface
    // tools are in the catalog but will not be exercised by the ScriptedModel.

    // A scripted model that gives a plain text response (it doesn't call
    // any MCP tools, proving the MCP integration didn't break anything).
    let model = ScriptedModel::new(vec![ModelResponse::final_text("MCP-ready mode active.")]);

    let agent = harness.agent(model).mcp(mcp).build().unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();

    let result = session.run("Hello").await.unwrap();
    println!("  Response: \"{}\"", result.text);
    assert_eq!(result.text, "MCP-ready mode active.");

    println!("  OK - Agent with MCP built and ran successfully\n");
}

// ---------------------------------------------------------------------------
// 4 · AGENT THAT USES CONNECTED MCP TOOLS
// ---------------------------------------------------------------------------

async fn agent_uses_mcp_tools(root: &std::path::Path) {
    println!("=== Agent That Can Call MCP Tools (scripted) ===");

    let harness = Harness::builder().workspace(root).build().unwrap();

    // Set up a connected MCP so the agent has access to mcp__svc__greet etc.
    let server_script = root.join("svc_server.sh");
    std::fs::write(
        &server_script,
        "\
#!/bin/sh\n\
read _initialize\n\
printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"svc\",\"version\":\"1.0\"}}}'\n\
read _initialized\n\
read _list\n\
printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"greet\",\"description\":\"Greets someone\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"name\":{\"type\":\"string\"}}}},{\"name\":\"factorial\",\"description\":\"Computes factorial\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"n\":{\"type\":\"integer\"}}}}]}}'\n\
",
    )
    .unwrap();

    let mcp = harness.mcp();
    let cmd = format!("sh {}", server_script.display());
    mcp.connect("svc", &cmd).await.unwrap();

    // Custom echo-tool so the scripted model can return deterministic results.
    // These FnTools shadow the McpClient-provided tools during execution.
    let greet_fn = FnTool::new(
        "mcp__svc__greet",
        "Greet a person by name via MCP server svc",
        json!({
            "type":"object",
            "properties":{"name":{"type":"string"}}
        }),
        |args, _ctx| async move {
            let name = args["name"].as_str().unwrap_or("friend");
            Ok(json!({"greeting": format!("Hello, {}!", name)}))
        },
    );

    let fact_fn = FnTool::new(
        "mcp__svc__factorial",
        "Compute n! via MCP server svc",
        json!({
            "type":"object",
            "properties":{"n":{"type":"integer"}}
        }),
        |args, _ctx| async move {
            let n = args["n"].as_i64().unwrap_or(0);
            let mut f: i64 = 1;
            for i in 2..=n {
                f *= i;
            }
            Ok(json!({"result": f}))
        },
    );

    // The scripted model asks for two MCP tool calls, then confirms the answers.
    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("Let me use the connected service.".into()),
            calls: vec![
                call("t1", "mcp__svc__greet", json!({"name": "Alice"})),
                call("t2", "mcp__svc__factorial", json!({"n": 5})),
            ],
            usage: Some(Usage {
                input_tokens: 15,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
            }),
        },
        ModelResponse::Final {
            text: "Greeting: Hello, Alice! Factorial of 5 is 120.".into(),
            usage: Some(Usage {
                input_tokens: 40,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
            }),
        },
    ]);

    let agent = harness
        .agent(model)
        .tool(greet_fn)
        .tool(fact_fn)
        .mcp(mcp.clone()) // adds interface tools + wraps model for MCP-aware dispatch
        .build()
        .unwrap();

    let session = agent.new_session().ephemeral().open().unwrap();
    let result = session.run("Greet Alice and compute 5!").await.unwrap();

    println!("  Response: \"{}\"", result.text);
    assert!(result.text.contains("Alice"));
    assert!(result.text.contains("120"));
    assert_eq!(result.metered_steps, 2);

    mcp.disconnect("svc");
    println!("  OK - Agent used MCP tools (scripted) successfully\n");
}

// ---------------------------------------------------------------------------
// MAIN
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = temp_dir("local-mcp");

    println!("MCP Examples --- workspace: {}\n", root.display());

    empty_mcp(&Harness::builder().workspace(&root).build()?);
    mock_stdio_server(&root).await;
    agent_with_mcp(&root).await;
    agent_uses_mcp_tools(&root).await;

    std::fs::remove_dir_all(&root)?;
    println!("Temp dir cleaned up.");
    Ok(())
}
