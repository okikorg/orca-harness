use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use orca_harness_tool_extensions::agent_plugins::load_agent_plugin;

use super::{PluginServer, PluginSnapshot};

impl PluginSnapshot {
    pub(super) fn load() -> Self {
        Self::from_registrations(crate::config::stored_plugins(), |name| {
            crate::config::plugin_data_path(name)
        })
    }

    pub(super) fn from_registrations(
        mut registrations: Vec<crate::config::RegisteredPlugin>,
        data_path: impl Fn(&str) -> io::Result<PathBuf>,
    ) -> Self {
        registrations.sort_by(|left, right| left.name.cmp(&right.name));
        let mut servers = Vec::new();
        let mut hooks = Vec::new();
        let mut hook_counts = BTreeMap::new();
        let mut hook_data = BTreeMap::new();
        let mut loaded = BTreeSet::new();
        let mut errors = BTreeMap::new();
        let mut mcp_warnings = BTreeMap::new();
        let mut lines = Vec::new();
        for registered in registrations.into_iter().filter(|plugin| plugin.enabled) {
            let data = match data_path(&registered.name) {
                Ok(data) => data,
                Err(error) => {
                    errors.insert(registered.name.clone(), error.to_string());
                    lines.push(format!("Plugin {} · {error}", registered.name));
                    continue;
                }
            };
            let plugin = match load_agent_plugin(&registered.root, &data) {
                Ok(plugin) => plugin,
                Err(error) => {
                    errors.insert(registered.name.clone(), error.to_string());
                    lines.push(format!("Plugin {} · {error}", registered.name));
                    continue;
                }
            };
            if plugin.name != registered.name {
                let error = format!(
                    "Plugin {} · registered root now declares plugin {}",
                    registered.name, plugin.name
                );
                errors.insert(registered.name.clone(), error.clone());
                lines.push(error);
                continue;
            }
            loaded.insert(registered.name.clone());
            if !plugin.hooks.is_empty() {
                hook_counts.insert(registered.name.clone(), plugin.hooks.len());
                hook_data.insert(registered.name.clone(), data.clone());
                hooks.extend(plugin.hooks.clone());
            }
            let warnings = plugin
                .warnings
                .iter()
                .filter(|warning| {
                    warning.scope == "mcp.json" || warning.scope.starts_with("mcpServers.")
                })
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            if !warnings.is_empty() {
                mcp_warnings.insert(registered.name.clone(), warnings);
            }
            lines.extend(
                plugin
                    .warnings
                    .iter()
                    .filter(|warning| !warning.scope.starts_with("skills."))
                    .map(|warning| format!("Plugin {} warning · {warning}", registered.name)),
            );
            let mut parsed = plugin.mcp_servers;
            parsed.sort_by(|left, right| left.server_name.cmp(&right.server_name));
            servers.extend(parsed.into_iter().map(|server| PluginServer {
                name: server.id,
                plugin: registered.name.clone(),
                server: server.server_name,
                data: data.clone(),
                launch: server.launch,
            }));
        }
        Self {
            servers,
            hooks,
            hook_counts,
            hook_data,
            loaded,
            errors,
            mcp_warnings,
            startup_lines: Arc::new(Mutex::new(Some(lines))),
        }
    }

    #[cfg(test)]
    pub(super) fn empty() -> Self {
        Self::default()
    }

    pub(super) fn take_startup_lines(&self) -> Vec<String> {
        self.startup_lines
            .lock()
            .expect("plugin startup lines lock")
            .take()
            .unwrap_or_default()
    }
}
