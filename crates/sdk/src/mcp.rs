use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use orca_harness_core::Tool;
use orca_harness_tool_extensions::mcp::{McpCatalog, McpClient};

use crate::SdkError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    pub name: String,
    pub healthy: bool,
    pub tool_count: usize,
}

#[derive(Clone, Default)]
pub struct Mcp {
    catalog: McpCatalog,
    servers: Arc<RwLock<BTreeMap<String, usize>>>,
}

impl Mcp {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn connect(&self, name: &str, command: &str) -> Result<McpServerStatus, SdkError> {
        if name.trim().is_empty() {
            return Err(SdkError::Config("MCP server name cannot be empty".into()));
        }
        let connection = McpClient::connect(name, command).await?;
        let count = connection.tools().len();
        self.catalog.insert(name.to_string(), connection)?;
        self.servers
            .write()
            .expect("mcp servers lock")
            .insert(name.to_string(), count);
        Ok(McpServerStatus {
            name: name.to_string(),
            healthy: true,
            tool_count: count,
        })
    }

    pub fn disconnect(&self, name: &str) -> bool {
        self.catalog.remove(name);
        self.servers
            .write()
            .expect("mcp servers lock")
            .remove(name)
            .is_some()
    }

    pub fn servers(&self) -> Vec<McpServerStatus> {
        self.servers
            .read()
            .expect("mcp servers lock")
            .iter()
            .map(|(name, count)| McpServerStatus {
                name: name.clone(),
                healthy: self.catalog.healthy(name),
                tool_count: *count,
            })
            .collect()
    }

    pub fn catalog(&self) -> McpCatalog {
        self.catalog.clone()
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools = self.catalog.interface_tools();
        for name in self.servers.read().expect("mcp servers lock").keys() {
            tools.extend(self.catalog.server_tools(name));
        }
        tools
    }
}
