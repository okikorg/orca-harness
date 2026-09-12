//! Standalone and Agent Plugin MCP tools behind one shared handle.
//!
//! `/mcp` edits standalone config and displays plugin servers read-only.
//! Plugin registrations are snapshotted when this manager is constructed, so
//! saved plugin changes apply next process while standalone reloads reconcile
//! against the same plugin state.
//!
//! Reloads are a diff, not a rebuild. Only added, removed, unhealthy, or
//! identity-changed servers reconnect. Failed connections retain their error
//! until desired state changes, and dropping a connection kills its process.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures_util::{stream, StreamExt};

use orca_harness_core::Tool;
use orca_harness_tool_extensions::agent_plugins::PluginHook;
use orca_harness_tool_extensions::mcp::{McpCatalog, McpClient, StdioLaunch};

const CONNECT_CONCURRENCY: usize = 4;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

mod hooks;
mod plugins;
pub(crate) mod view;

/// What the last connection attempt for a server produced in `/mcp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpState {
    Connected(usize),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginRuntimeState {
    NotLoaded,
    Starting,
    Loaded {
        servers: usize,
        tools: usize,
    },
    Degraded {
        connected: usize,
        servers: usize,
        tools: usize,
        warning: String,
    },
    Failed {
        connected: usize,
        servers: usize,
        tools: usize,
        error: String,
    },
}

struct Connection {
    desired: DesiredServer,
    tools: Vec<Arc<dyn Tool>>,
    error: Option<String>,
}

/// Complete process identity used by the reload diff. Legacy command strings
/// retain their original launch and inherited-environment behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LaunchIdentity {
    Legacy(String),
    Structured(StdioLaunch),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DesiredSource {
    Standalone,
    Plugin {
        plugin: String,
        server: String,
        data: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesiredServer {
    name: String,
    launch: LaunchIdentity,
    source: DesiredSource,
}

#[derive(Debug, Clone)]
struct PluginServer {
    name: String,
    plugin: String,
    server: String,
    data: PathBuf,
    launch: StdioLaunch,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PluginCollision {
    plugin: String,
    server: String,
    id: String,
}

#[derive(Clone, Default)]
struct PluginSnapshot {
    servers: Vec<PluginServer>,
    hooks: Vec<PluginHook>,
    hook_counts: BTreeMap<String, usize>,
    hook_data: BTreeMap<String, PathBuf>,
    loaded: BTreeSet<String>,
    errors: BTreeMap<String, String>,
    mcp_warnings: BTreeMap<String, Vec<String>>,
    startup_lines: Arc<Mutex<Option<Vec<String>>>>,
}

/// Shared MCP handle used by builders, workers, providers, subagents, and pickers.
#[derive(Clone)]
pub struct McpServers {
    connections: Arc<RwLock<HashMap<String, Connection>>>,
    catalog: McpCatalog,
    desired: Arc<RwLock<Vec<DesiredServer>>>,
    plugin_collisions: Arc<Mutex<BTreeSet<PluginCollision>>>,
    plugins: PluginSnapshot,
}

impl Default for McpServers {
    fn default() -> Self {
        Self::new()
    }
}

impl McpServers {
    pub fn new() -> Self {
        Self::with_plugin_snapshot(PluginSnapshot::load())
    }

    fn with_plugin_snapshot(plugins: PluginSnapshot) -> Self {
        Self {
            connections: Arc::default(),
            catalog: McpCatalog::new(),
            desired: Arc::default(),
            plugin_collisions: Arc::default(),
            plugins,
        }
    }

    /// The three stable interfaces followed by every hidden remote tool in
    /// deterministic desired-server order. Existing agent builders register
    /// this exact set, so plugin tools inherit the same gates and wrappers.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools = self.catalog.interface_tools();
        let desired = self.desired.read().expect("mcp desired lock");
        tools.extend(
            desired
                .iter()
                .flat_map(|server| self.catalog.server_tools(&server.name)),
        );
        tools
    }

    pub fn catalog(&self) -> McpCatalog {
        self.catalog.clone()
    }

    pub fn state(&self, name: &str) -> Option<McpState> {
        let connections = self.connections.read().expect("mcp lock");
        let connection = connections.get(name)?;
        Some(match &connection.error {
            Some(err) => McpState::Failed(err.clone()),
            None if !self.catalog.healthy(name) => {
                McpState::Failed("connection interrupted; reload to reconnect".into())
            }
            None => McpState::Connected(connection.tools.len()),
        })
    }

    /// Aggregate one plugin's process snapshot for the `/plugin` picker.
    /// Saved enablement is intentionally left to the caller so configuration
    /// and what this already-running process loaded remain visibly distinct.
    pub fn plugin_state(&self, name: &str) -> PluginRuntimeState {
        if let Some(error) = self.plugins.errors.get(name) {
            return PluginRuntimeState::Failed {
                connected: 0,
                servers: 0,
                tools: 0,
                error: error.clone(),
            };
        }
        if !self.plugins.loaded.contains(name) {
            return PluginRuntimeState::NotLoaded;
        }
        let plugin_servers = self
            .plugins
            .servers
            .iter()
            .filter(|server| server.plugin == name)
            .collect::<Vec<_>>();
        let mcp_warnings = self
            .plugins
            .mcp_warnings
            .get(name)
            .cloned()
            .unwrap_or_default();
        if plugin_servers.is_empty() {
            return if mcp_warnings.is_empty() {
                PluginRuntimeState::Loaded {
                    servers: 0,
                    tools: 0,
                }
            } else {
                PluginRuntimeState::Degraded {
                    connected: 0,
                    servers: 0,
                    tools: 0,
                    warning: mcp_warnings.join("; "),
                }
            };
        }

        let collisions = self
            .plugin_collisions
            .lock()
            .expect("plugin collision lock");
        let connections = self.connections.read().expect("mcp lock");
        let mut connected = 0;
        let mut tools = 0;
        let mut pending = false;
        let mut failures = Vec::new();
        for server in &plugin_servers {
            if collisions
                .iter()
                .any(|collision| collision.plugin == name && collision.server == server.server)
            {
                failures.push(format!("{}: server ID collision", server.server));
                continue;
            }
            let Some(connection) = connections.get(&server.name) else {
                pending = true;
                continue;
            };
            match &connection.error {
                Some(error) => failures.push(format!("{}: {error}", server.server)),
                None if !self.catalog.healthy(&server.name) => {
                    failures.push(format!("{}: connection interrupted", server.server))
                }
                None => {
                    connected += 1;
                    tools += connection.tools.len();
                }
            }
        }
        if !failures.is_empty() {
            return PluginRuntimeState::Failed {
                connected,
                servers: plugin_servers.len(),
                tools,
                error: failures.join("; "),
            };
        }
        if pending {
            return PluginRuntimeState::Starting;
        }
        if !mcp_warnings.is_empty() {
            return PluginRuntimeState::Degraded {
                connected,
                servers: plugin_servers.len(),
                tools,
                warning: mcp_warnings.join("; "),
            };
        }
        PluginRuntimeState::Loaded {
            servers: connected,
            tools,
        }
    }

    /// Reconcile standalone config and the process's fixed plugin snapshot.
    /// Launch failures are isolated and connections overlap within a bounded
    /// window; ordered publication keeps catalog collision winners stable.
    pub async fn reload(&self) -> Vec<String> {
        self.reload_with_timeout(CONNECT_TIMEOUT).await
    }

    async fn reload_with_timeout(&self, timeout: Duration) -> Vec<String> {
        let configured = crate::config::stored_mcp_servers();
        let (desired, collisions) = self.desired_servers(&configured);
        let previous_desired = self.desired.read().expect("mcp desired lock").clone();
        let desired_changed = previous_desired != desired;
        let mut lines = self.plugins.take_startup_lines();
        self.report_new_collisions(collisions, &mut lines);

        // Drop first, so removed or changed processes are gone before their
        // replacements start. Catalog-duplicate failures retry only when the
        // surrounding desired set changes and may have released the name.
        let mut stale = {
            let connections = self.connections.read().expect("mcp lock");
            connections
                .iter()
                .filter(|(name, connection)| {
                    desired
                        .iter()
                        .find(|server| server.name == **name)
                        .is_none_or(|server| server != &connection.desired)
                        || (connection.error.is_none() && !self.catalog.healthy(name))
                        || (desired_changed
                            && connection
                                .error
                                .as_ref()
                                .is_some_and(|error| error.contains("duplicate MCP tool name")))
                })
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>()
        };
        stale.sort();
        if !stale.is_empty() {
            let mut connections = self.connections.write().expect("mcp lock");
            for name in &stale {
                connections.remove(name);
                self.catalog.remove(name);
                if configured.iter().any(|server| &server.name == name) {
                    lines.push(format!("MCP {name} disconnected"));
                }
            }
        }

        let pending = {
            let connections = self.connections.read().expect("mcp lock");
            desired
                .iter()
                .filter(|server| !connections.contains_key(&server.name))
                .cloned()
                .collect::<Vec<_>>()
        };
        // Poll launches concurrently, but publish in desired order so notices
        // and duplicate-tool winners never depend on handshake timing. Keeping
        // futures here (not detached tasks) also preserves drop cancellation.
        let mut connecting = stream::iter(pending)
            .map(|server| async move {
                let connected = tokio::time::timeout(timeout, self.connect(&server))
                    .await
                    .unwrap_or_else(|_| {
                        Err(format!(
                            "connection timed out after {timeout:?}; check the server command, environment, and startup logs, then disable/re-enable it in /mcp to retry"
                        ))
                    });
                (server, connected)
            })
            .buffered(CONNECT_CONCURRENCY);
        while let Some((server, connected)) = connecting.next().await {
            let connection = match connected {
                Ok(connected) => {
                    let count = match connected.tools().len() {
                        1 => "1 tool".to_string(),
                        count => format!("{count} tools"),
                    };
                    let tools = connected
                        .tools()
                        .iter()
                        .cloned()
                        .map(|tool| tool as Arc<dyn Tool>)
                        .collect();
                    match self.catalog.insert(server.name.clone(), connected) {
                        Ok(()) => {
                            lines.push(server.connected_line(&count));
                            Connection {
                                desired: server.clone(),
                                tools,
                                error: None,
                            }
                        }
                        Err(error) => {
                            lines.push(server.error_line(&error.to_string()));
                            Connection {
                                desired: server.clone(),
                                tools: Vec::new(),
                                error: Some(error.to_string()),
                            }
                        }
                    }
                }
                Err(error) => {
                    lines.push(server.error_line(&error.to_string()));
                    Connection {
                        desired: server.clone(),
                        tools: Vec::new(),
                        error: Some(error.to_string()),
                    }
                }
            };
            self.connections
                .write()
                .expect("mcp lock")
                .insert(server.name.clone(), connection);
        }
        drop(connecting);
        self.catalog.reorder(
            &desired
                .iter()
                .map(|server| server.name.clone())
                .collect::<Vec<_>>(),
        );
        *self.desired.write().expect("mcp desired lock") = desired;
        lines
    }

    async fn connect(
        &self,
        server: &DesiredServer,
    ) -> Result<orca_harness_tool_extensions::mcp::McpConnection, String> {
        match &server.launch {
            LaunchIdentity::Legacy(command) => {
                crate::config::validate_mcp_server(&server.name, command)
                    .map_err(|error| error.to_string())?;
                McpClient::connect(&server.name, command)
                    .await
                    .map_err(|error| error.to_string())
            }
            LaunchIdentity::Structured(launch) => {
                let DesiredSource::Plugin { data, .. } = &server.source else {
                    unreachable!("structured launches are plugin-owned")
                };
                crate::config::create_plugin_data_dir(data)
                    .map_err(|error| format!("cannot create plugin data directory: {error}"))?;
                McpClient::connect_stdio(&server.name, launch)
                    .await
                    .map_err(|error| error.to_string())
            }
        }
    }

    fn desired_servers(
        &self,
        configured: &[crate::config::McpServer],
    ) -> (Vec<DesiredServer>, BTreeSet<PluginCollision>) {
        let mut desired = configured
            .iter()
            .filter(|server| server.enabled)
            .map(|server| DesiredServer {
                name: server.name.clone(),
                launch: LaunchIdentity::Legacy(server.command.clone()),
                source: DesiredSource::Standalone,
            })
            .collect::<Vec<_>>();
        desired.extend(self.plugins.servers.iter().map(|server| DesiredServer {
            name: server.name.clone(),
            launch: LaunchIdentity::Structured(server.launch.clone()),
            source: DesiredSource::Plugin {
                plugin: server.plugin.clone(),
                server: server.server.clone(),
                data: server.data.clone(),
            },
        }));

        let mut counts = HashMap::<String, usize>::new();
        for server in &desired {
            *counts.entry(server.name.clone()).or_default() += 1;
        }
        let collisions = desired
            .iter()
            .filter_map(|server| {
                let DesiredSource::Plugin {
                    plugin,
                    server: plugin_server,
                    ..
                } = &server.source
                else {
                    return None;
                };
                (counts[&server.name] > 1).then(|| PluginCollision {
                    plugin: plugin.clone(),
                    server: plugin_server.clone(),
                    id: server.name.clone(),
                })
            })
            .collect::<BTreeSet<_>>();
        desired.retain(|server| {
            let DesiredSource::Plugin {
                plugin,
                server: plugin_server,
                ..
            } = &server.source
            else {
                return true;
            };
            !collisions.contains(&PluginCollision {
                plugin: plugin.clone(),
                server: plugin_server.clone(),
                id: server.name.clone(),
            })
        });
        (desired, collisions)
    }

    fn report_new_collisions(
        &self,
        collisions: BTreeSet<PluginCollision>,
        lines: &mut Vec<String>,
    ) {
        let mut previous = self
            .plugin_collisions
            .lock()
            .expect("plugin collision lock");
        lines.extend(collisions.difference(&previous).map(|collision| {
            format!(
                "Plugin {} MCP {} · desired server ID collision: {}",
                collision.plugin, collision.server, collision.id
            )
        }));
        *previous = collisions;
    }
}

impl DesiredServer {
    fn connected_line(&self, count: &str) -> String {
        match &self.source {
            DesiredSource::Standalone => format!("MCP {} connected · {count}", self.name),
            DesiredSource::Plugin { plugin, server, .. } => {
                format!("Plugin {plugin} MCP {server} connected · {count}")
            }
        }
    }

    fn error_line(&self, error: &str) -> String {
        match &self.source {
            DesiredSource::Standalone => format!("MCP {} · {error}", self.name),
            DesiredSource::Plugin { plugin, server, .. } => {
                format!("Plugin {plugin} MCP {server} · {error}")
            }
        }
    }
}

#[cfg(test)]
#[path = "mcp/tests.rs"]
mod tests;
