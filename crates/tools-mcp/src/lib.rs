//! # Orca Harness MCP Tools
//!
//! A minimal Model Context Protocol client, deliberately outside the
//! core tool set: it spawns host-configured server processes and opens
//! whatever capabilities they carry — an explicit host opt-in, not a
//! kernel assumption.
//!
//! [`McpClient::connect`] launches one stdio MCP server (JSON-RPC 2.0,
//! one message per line), performs the `initialize` handshake, lists the
//! server's tools, and returns each as an [`orca_harness_core::Tool`]
//! named `mcp__<server>__<tool>`. The connection lives as long as its
//! tools; dropping the last one kills the server process.
//!
//! ```no_run
//! use orca_harness_core::Agent;
//! use orca_harness_tools_mcp::McpClient;
//!
//! # async fn example(model: impl orca_harness_core::Model) -> Result<(), Box<dyn std::error::Error>> {
//! let mut agent = Agent::new(model);
//! for tool in McpClient::connect("docs", "npx -y @modelcontextprotocol/server-everything").await? {
//!     agent = agent.tool_arc(tool);
//! }
//! # let _ = agent; Ok(()) }
//! ```

mod client;
mod tool;

pub use client::{McpClient, McpError};
pub use tool::McpTool;
