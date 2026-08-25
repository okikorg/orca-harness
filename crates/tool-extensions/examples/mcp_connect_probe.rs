//! Live smoke test against a real MCP server: connect, print the tool
//! list, and (optionally) select and call one tool.

use orca_harness_core::{CancellationToken, Tool, ToolContext};
use orca_harness_tool_extensions::mcp::{McpCatalog, McpClient};

fn context(tool_name: &str) -> ToolContext {
    ToolContext {
        call_id: "probe-1".into(),
        tool_name: tool_name.into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let server = args.next().unwrap_or_else(|| "everything".into());
    let command = args
        .next()
        .unwrap_or_else(|| "npx -y @modelcontextprotocol/server-everything".into());

    let connection = McpClient::connect(&server, &command).await?;
    println!("{}: {} tools", server, connection.tools().len());
    for tool in connection.tools() {
        let schema = tool.schema();
        println!("  {}  {}", schema.name, schema.description);
    }

    let (Some(name), Some(input)) = (args.next(), args.next()) else {
        return Ok(());
    };
    let catalog = McpCatalog::new();
    catalog.insert(server.clone(), connection)?;
    let selector = catalog
        .interface_tools()
        .into_iter()
        .find(|tool| tool.schema().name == "mcp_select_tool")
        .expect("selector registered");
    selector
        .call(
            serde_json::json!({ "name": name }),
            &context("mcp_select_tool"),
        )
        .await?;
    let tool = catalog
        .server_tools(&server)
        .into_iter()
        .find(|tool| tool.schema().name == name)
        .ok_or_else(|| format!("no tool named {name}"))?;
    let output = tool
        .call(serde_json::from_str(&input)?, &context(&name))
        .await?;
    println!("--- {name}: {output}");
    Ok(())
}
