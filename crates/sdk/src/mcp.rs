use std::sync::Arc;

use orca_harness_core::Tool;
use orca_harness_tool_extensions::mcp::{McpCatalog, McpClient};

use crate::SdkError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    pub name: String,
    pub healthy: bool,
    pub tool_count: usize,
}

/// The MCP server registry an agent reads at each run boundary. Clones
/// share one catalog, so a handle given to [`AgentBuilder::mcp`] keeps
/// working after the agent is built.
///
/// A run captures the MCP tool set (the three interface tools plus every
/// connected server's tools) when it starts and keeps it for the whole
/// run, so the schemas a run registers never change under it. `connect`,
/// `connect_stdio`, and `disconnect` take effect at the next run of any
/// session using the handle. A server disconnected or replaced during a
/// run stays callable through that run's captured tools: in-flight and
/// later calls in the run go to the old connection and either succeed or
/// fail with that connection's transport error, never with an unknown
/// tool. The catalog does drive schema visibility live (see
/// [`McpModel`](orca_harness_tool_extensions::mcp::McpModel)), so a
/// replaced server's schemas can drop out of the model's view mid-run
/// until selected again. Nothing else changes mid-run: there is no
/// mutation of a run's registry.
///
/// [`AgentBuilder::mcp`]: crate::AgentBuilder::mcp
#[derive(Clone, Default)]
pub struct Mcp {
    catalog: McpCatalog,
}

impl Mcp {
    pub fn new() -> Self {
        Self::default()
    }

    /// Connect a stdio server (`command` split on whitespace, no shell)
    /// under `name`, replacing a same-named server in place.
    pub async fn connect(&self, name: &str, command: &str) -> Result<McpServerStatus, SdkError> {
        if name.trim().is_empty() {
            return Err(SdkError::Config("MCP server name cannot be empty".into()));
        }
        let connection = McpClient::connect(name, command).await?;
        self.register(name, connection)
    }

    /// Connect a structured stdio launch without shell/string conversion.
    pub async fn connect_stdio(
        &self,
        name: &str,
        launch: &orca_harness_tool_extensions::mcp::StdioLaunch,
    ) -> Result<McpServerStatus, SdkError> {
        if name.trim().is_empty() {
            return Err(SdkError::Config("MCP server name cannot be empty".into()));
        }
        let connection = McpClient::connect_stdio(name, launch).await?;
        self.register(name, connection)
    }

    fn register(
        &self,
        name: &str,
        connection: orca_harness_tool_extensions::mcp::McpConnection,
    ) -> Result<McpServerStatus, SdkError> {
        self.catalog.insert(name.to_string(), connection)?;
        Ok(self.status(name))
    }

    /// Drop a server from the catalog; returns whether it was connected.
    /// Runs that already captured its tools keep them (see the type docs).
    pub fn disconnect(&self, name: &str) -> bool {
        self.catalog.remove(name)
    }

    /// Every connected server, in catalog order, read live from the
    /// catalog: a reconnect is reflected at once.
    pub fn servers(&self) -> Vec<McpServerStatus> {
        self.catalog
            .servers()
            .iter()
            .map(|name| self.status(name))
            .collect()
    }

    fn status(&self, name: &str) -> McpServerStatus {
        McpServerStatus {
            name: name.to_string(),
            healthy: self.catalog.healthy(name),
            tool_count: self.catalog.server_tools(name).len(),
        }
    }

    pub fn catalog(&self) -> McpCatalog {
        self.catalog.clone()
    }

    /// The tool set as the catalog stands now: the interface tools, then
    /// each server's tools in catalog order. A run takes this once at its
    /// start.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools = self.catalog.interface_tools();
        for name in self.catalog.servers() {
            tools.extend(self.catalog.server_tools(&name));
        }
        tools
    }
}
