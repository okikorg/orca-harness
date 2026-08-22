//! The configured MCP servers' tools, behind a shared handle. `/mcp`
//! edits the config file and the worker reconnects and rebuilds, so
//! changes apply to the next run — the same shape as extension toggles.
//! Connections live as long as their tools: when a rebuild replaces the
//! agent, dropped tools kill the old server processes.
//!
//! Reloads are a diff, not a rebuild. Reconnecting is seconds per
//! server (`npx` launchers especially), so toggling one server in the
//! overlay must not respawn the other six: only names that were added,
//! removed, or had their command changed are touched, and only those
//! report a status line. A server that failed to connect keeps its
//! error until something about it changes — toggling it off and on
//! retries.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use orca_harness_core::Tool;
use orca_harness_tools_mcp::McpClient;

/// What the last connection attempt for a server produced. Rendered by
/// the /mcp overlay next to each row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpState {
    Connected(usize),
    Failed(String),
}

/// One live (or failed) connection. `command` is what it was connected
/// with, so a config edit is detectable without reconnecting.
struct Connection {
    command: String,
    tools: Vec<Arc<dyn Tool>>,
    error: Option<String>,
}

/// Cloneable handle captured by the agent-build closure (like
/// `TruncationStore`): the worker reloads it, `build_agent` reads it,
/// and the TUI reads per-server state for the /mcp overlay.
#[derive(Clone, Default)]
pub struct McpServers {
    connections: Arc<RwLock<HashMap<String, Connection>>>,
}

impl McpServers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tools from every connected server, in config order so the tool
    /// list a model sees does not shuffle between rebuilds.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let connections = self.connections.read().expect("mcp lock");
        crate::config::stored_mcp_servers()
            .iter()
            .filter_map(|server| connections.get(&server.name))
            .flat_map(|connection| connection.tools.iter().cloned())
            .collect()
    }

    /// The last attempt's outcome for `name`, or `None` when the server
    /// is disabled or has not been connected yet.
    pub fn state(&self, name: &str) -> Option<McpState> {
        let connections = self.connections.read().expect("mcp lock");
        let connection = connections.get(name)?;
        Some(match &connection.error {
            Some(err) => McpState::Failed(err.clone()),
            None => McpState::Connected(connection.tools.len()),
        })
    }

    /// Bring the live connections in line with the config: connect what
    /// is new or changed, drop what was removed or disabled, leave the
    /// rest running. Returns one transcript status line per *changed*
    /// server; an unchanged config reports nothing. A server that fails
    /// to connect loses its tools but never stops the others.
    pub async fn reload(&self) -> Vec<String> {
        let configured = crate::config::stored_mcp_servers();

        // Drop first, so a removed server's process is gone before the
        // replacement for a renamed/edited one spawns.
        let mut lines = Vec::new();
        let stale: Vec<String> = {
            let connections = self.connections.read().expect("mcp lock");
            connections
                .iter()
                .filter(|(name, connection)| {
                    !configured.iter().any(|server| {
                        server.enabled
                            && server.name == **name
                            && server.command == connection.command
                    })
                })
                .map(|(name, _)| name.clone())
                .collect()
        };
        if !stale.is_empty() {
            let mut connections = self.connections.write().expect("mcp lock");
            for name in &stale {
                connections.remove(name);
                // A server that is merely gone from the config was
                // reported by /mcp remove already; only disabling and
                // re-command are worth a line here.
                if configured.iter().any(|s| &s.name == name) {
                    lines.push(format!("mcp {name}: disconnected"));
                }
            }
        }

        for server in configured.iter().filter(|s| s.enabled) {
            if self
                .connections
                .read()
                .expect("mcp lock")
                .contains_key(&server.name)
            {
                continue;
            }
            let connection = match McpClient::connect(&server.name, &server.command).await {
                Ok(tools) => {
                    let count = match tools.len() {
                        1 => "1 tool".to_string(),
                        n => format!("{n} tools"),
                    };
                    lines.push(format!("mcp {}: connected, {count}", server.name));
                    Connection {
                        command: server.command.clone(),
                        tools,
                        error: None,
                    }
                }
                Err(err) => {
                    lines.push(format!("mcp {}: {err}", server.name));
                    Connection {
                        command: server.command.clone(),
                        tools: Vec::new(),
                        error: Some(err.to_string()),
                    }
                }
            };
            self.connections
                .write()
                .expect("mcp lock")
                .insert(server.name.clone(), connection);
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reload against an empty config clears the set quietly; a
    /// misconfigured server reports and is skipped, never fatal.
    #[tokio::test]
    async fn reload_reports_per_server_and_replaces_the_set() {
        let servers = McpServers::new();
        assert!(servers.reload().await.is_empty());
        assert!(servers.tools().is_empty());

        crate::config::save_mcp_server("ghost", "orca-no-such-binary-xyz").unwrap();
        let lines = servers.reload().await;
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("mcp ghost:"), "line: {}", lines[0]);
        assert!(servers.tools().is_empty());
        assert!(matches!(servers.state("ghost"), Some(McpState::Failed(_))));
    }

    /// The diff: an unchanged config is a no-op, so toggling one server
    /// never re-handshakes the others. Disabling drops the connection;
    /// re-enabling retries it.
    #[tokio::test]
    async fn reload_only_touches_servers_that_changed() {
        let servers = McpServers::new();
        crate::config::save_mcp_server("ghost", "orca-no-such-binary-xyz").unwrap();
        assert_eq!(servers.reload().await.len(), 1);

        // Second reload with the same config: nothing reconnects, and
        // the recorded failure is kept rather than retried.
        assert!(servers.reload().await.is_empty());
        assert!(matches!(servers.state("ghost"), Some(McpState::Failed(_))));

        crate::config::set_mcp_enabled("ghost", false).unwrap();
        assert_eq!(servers.reload().await, ["mcp ghost: disconnected"]);
        assert_eq!(servers.state("ghost"), None);

        crate::config::set_mcp_enabled("ghost", true).unwrap();
        assert_eq!(servers.reload().await.len(), 1);
        assert!(matches!(servers.state("ghost"), Some(McpState::Failed(_))));

        // Removal drops the connection without a "disconnected" line —
        // /mcp remove already reported it.
        crate::config::remove_mcp_server("ghost").unwrap();
        assert!(servers.reload().await.is_empty());
        assert_eq!(servers.state("ghost"), None);
    }
}
