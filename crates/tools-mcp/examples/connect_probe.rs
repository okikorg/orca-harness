//! Live smoke test against a real MCP server: connect, print the tool
//! list, and (optionally) call one tool.
//!
//! ```bash
//! cargo run -p orca-harness-tools-mcp --example connect_probe -- \
//!     everything "npx -y @modelcontextprotocol/server-everything"
//! cargo run -p orca-harness-tools-mcp --example connect_probe -- \
//!     everything "npx -y @modelcontextprotocol/server-everything" \
//!     mcp__everything__echo '{"message": "hi"}'
//! ```

use orca_harness_core::{CancellationToken, ToolContext};
use orca_harness_tools_mcp::McpClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let server = args.next().unwrap_or_else(|| "everything".into());
    let command = args
        .next()
        .unwrap_or_else(|| "npx -y @modelcontextprotocol/server-everything".into());

    let tools = McpClient::connect(&server, &command).await?;
    println!("{}: {} tools", server, tools.len());
    for tool in &tools {
        let schema = tool.schema();
        println!("  {}  {}", schema.name, schema.description);
    }

    let (Some(name), Some(input)) = (args.next(), args.next()) else {
        return Ok(());
    };
    let tool = tools
        .iter()
        .find(|t| t.schema().name == name)
        .ok_or_else(|| format!("no tool named {name}"))?;
    let ctx = ToolContext {
        call_id: "probe-1".into(),
        tool_name: name.clone(),
        cancellation: CancellationToken::new(),
        deadline: None,
    };
    let output = tool.call(serde_json::from_str(&input)?, &ctx).await?;
    println!("--- {name}: {output}");
    Ok(())
}
