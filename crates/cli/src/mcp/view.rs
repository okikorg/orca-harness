//! Read-only runtime projections used by startup and the unified `/mcp` picker.

use std::collections::BTreeSet;

use super::{McpServers, McpState};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PluginMcpEntry {
    pub id: String,
    pub plugin: String,
    pub server: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StartupInventory {
    pub plugins: usize,
    pub servers: usize,
    pub tools: usize,
    pub connected_notices: BTreeSet<String>,
}

impl PluginMcpEntry {
    pub fn label(&self) -> String {
        format!("{}/{}", self.plugin, self.server)
    }
}

impl McpServers {
    pub(crate) fn startup_inventory(&self) -> StartupInventory {
        let connections = self.connections.read().expect("mcp lock");
        let connected = connections
            .iter()
            .filter(|(name, connection)| connection.error.is_none() && self.catalog.healthy(name))
            .collect::<Vec<_>>();
        let connected_notices = connected
            .iter()
            .map(|(_, connection)| {
                let count = match connection.tools.len() {
                    1 => "1 tool".to_string(),
                    count => format!("{count} tools"),
                };
                connection.desired.connected_line(&count)
            })
            .collect();
        StartupInventory {
            plugins: self.plugins.loaded.len(),
            servers: connected.len(),
            tools: connected
                .iter()
                .map(|(_, connection)| connection.tools.len())
                .sum(),
            connected_notices,
        }
    }

    pub(crate) fn plugin_mcp_entries(&self) -> Vec<PluginMcpEntry> {
        self.plugins
            .servers
            .iter()
            .map(|server| PluginMcpEntry {
                id: server.name.clone(),
                plugin: server.plugin.clone(),
                server: server.server.clone(),
            })
            .collect()
    }

    pub(crate) fn plugin_mcp_state(&self, entry: &PluginMcpEntry) -> Option<McpState> {
        let collisions = self
            .plugin_collisions
            .lock()
            .expect("plugin collision lock");
        if collisions
            .iter()
            .any(|collision| collision.plugin == entry.plugin && collision.server == entry.server)
        {
            return Some(McpState::Failed("server ID collision".into()));
        }
        drop(collisions);
        self.state(&entry.id)
    }
}
