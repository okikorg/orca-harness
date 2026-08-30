use std::collections::BTreeSet;
use std::sync::Arc;

use orca_harness_tool_extensions::plugin_hooks::PluginHookExtension;

use super::McpServers;

impl McpServers {
    /// Prepare enabled plugin hook data directories and build the one shared
    /// lifecycle extension. A data failure disables only that plugin's hooks.
    pub fn plugin_hook_extension(&self) -> (Option<Arc<PluginHookExtension>>, Vec<String>) {
        let mut unavailable = BTreeSet::new();
        let mut notices = Vec::new();
        for (plugin, data) in &self.plugins.hook_data {
            if let Err(error) = crate::config::create_plugin_data_dir(data) {
                unavailable.insert(plugin.clone());
                notices.push(format!(
                    "Plugin {plugin} hooks disabled · cannot create plugin data directory: {error}"
                ));
            }
        }
        let hooks = self
            .plugins
            .hooks
            .iter()
            .filter(|hook| !unavailable.contains(&hook.plugin_name))
            .cloned()
            .collect::<Vec<_>>();
        if hooks.is_empty() {
            (None, notices)
        } else {
            (Some(Arc::new(PluginHookExtension::new(hooks))), notices)
        }
    }

    pub fn plugin_hook_count(&self, plugin: &str) -> usize {
        self.plugins.hook_counts.get(plugin).copied().unwrap_or(0)
    }
}
