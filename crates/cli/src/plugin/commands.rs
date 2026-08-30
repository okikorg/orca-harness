use std::path::Path;

use orca_harness_core::Tool;
use orca_harness_tool_extensions::agent_plugins::{load_agent_plugin, AgentPlugin};
use orca_harness_tool_extensions::mcp::McpClient;

use super::{Language, PluginCommand};

pub(super) async fn run(command: PluginCommand) -> Result<(), String> {
    let lines = match command {
        PluginCommand::Init { name, language } => {
            let target = super::scaffold::create(&name, language)?;
            vec![format!("created plugin scaffold at {}", target.display())]
        }
        PluginCommand::Validate { path } => validate(&path)?,
        PluginCommand::Test { path } => test(&path).await?,
        PluginCommand::Install { path } => install(&path)?,
        PluginCommand::List => list(),
        PluginCommand::Inspect { name } => inspect(&name)?,
        PluginCommand::Enable { name } => enable(&name)?,
        PluginCommand::Disable { name } => disable(&name)?,
        PluginCommand::Uninstall { name } => uninstall(&name)?,
    };
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

pub(crate) fn init_in(base: &Path, name: &str, language: Language) -> Result<Vec<String>, String> {
    let target = super::scaffold::create_in(base, name, language)?;
    Ok(vec![format!(
        "created plugin scaffold at {}",
        target.display()
    )])
}

fn validation_data_path() -> Result<std::path::PathBuf, String> {
    crate::config::plugin_data_path("validation-probe").map_err(|error| error.to_string())
}

fn load_for_validation(path: &Path) -> Result<AgentPlugin, String> {
    load_agent_plugin(path, &validation_data_path()?).map_err(|error| error.to_string())
}

fn warning_lines(plugin: &AgentPlugin) -> Vec<String> {
    plugin
        .warnings
        .iter()
        .map(|warning| format!("warning: {warning}"))
        .collect()
}

pub(crate) fn validate(path: &Path) -> Result<Vec<String>, String> {
    let plugin = load_for_validation(path)?;
    let mut lines = warning_lines(&plugin);
    lines.extend(
        plugin
            .skills
            .skills
            .iter()
            .map(|skill| format!("skill: {}", skill.name)),
    );
    lines.push(format!(
        "valid plugin: {} ({} skill(s), {} stdio MCP server(s), {} Orcacode hook(s)) · {}",
        plugin.name,
        plugin.skills.skills.len(),
        plugin.mcp_servers.len(),
        plugin.hooks.len(),
        plugin.root.display()
    ));
    Ok(lines)
}

pub(crate) async fn test(path: &Path) -> Result<Vec<String>, String> {
    let initial = load_for_validation(path)?;
    let data = crate::config::plugin_data_path(&initial.name).map_err(|error| error.to_string())?;
    let plugin = load_agent_plugin(&initial.root, &data).map_err(|error| error.to_string())?;
    let mut lines = warning_lines(&plugin);
    lines.extend(
        plugin
            .skills
            .skills
            .iter()
            .map(|skill| format!("skill {}: valid", skill.name)),
    );
    if !plugin.mcp_servers.is_empty() || !plugin.hooks.is_empty() {
        crate::config::create_plugin_data_dir(&data)
            .map_err(|error| format!("cannot create plugin data directory: {error}"))?;
    }
    let mut failures = Vec::new();
    for server in &plugin.mcp_servers {
        match McpClient::connect_stdio(&server.id, &server.launch).await {
            Ok(connection) => {
                lines.push(format!("server {}:", server.server_name));
                for tool in connection.tools() {
                    lines.push(format!("  {}", tool.schema().name));
                }
                drop(connection);
            }
            Err(error) => failures.push(format!("{}: {error}", server.server_name)),
        }
    }
    for hook in &plugin.hooks {
        match orca_harness_tool_extensions::plugin_hooks::probe_hook(hook).await {
            Ok(()) => lines.push(format!(
                "hook {} #{}: valid",
                hook.event.as_str(),
                hook.index + 1
            )),
            Err(error) => failures.push(error),
        }
    }
    if plugin.mcp_servers.is_empty() {
        lines.push("no supported stdio MCP servers to execute".into());
    }
    if !failures.is_empty() {
        return Err(format!(
            "plugin executable test failed ({}); install dependencies and build required output first",
            failures.join("; ")
        ));
    }
    lines.push(format!("plugin test passed: {}", plugin.name));
    Ok(lines)
}

pub(crate) async fn test_report(path: &Path) -> Vec<String> {
    test(path)
        .await
        .unwrap_or_else(|error| vec![format!("plugin test failed: {error}")])
}

pub(crate) fn install(path: &Path) -> Result<Vec<String>, String> {
    let plugin = load_for_validation(path)?;
    let mut lines = warning_lines(&plugin);
    if let Some(existing) = crate::config::stored_plugin(&plugin.name) {
        if existing.root == plugin.root {
            lines.push(format!("plugin already installed: {}", plugin.name));
            return Ok(lines);
        }
        return Err(format!(
            "plugin {} is already registered from a different root: {}",
            plugin.name,
            existing.root.display()
        ));
    }
    crate::config::save_plugin(&plugin.name, &plugin.root, false)
        .map_err(|error| format!("cannot save plugin registration: {error}"))?;
    lines.push(format!("installed plugin {} (disabled)", plugin.name));
    Ok(lines)
}

fn list() -> Vec<String> {
    crate::config::stored_plugins()
        .into_iter()
        .map(|plugin| {
            let state = if plugin.enabled {
                "enabled"
            } else {
                "disabled"
            };
            format!("{}\t{}\t{}", plugin.name, state, plugin.root.display())
        })
        .collect()
}

pub(crate) fn inspect(name: &str) -> Result<Vec<String>, String> {
    let registered = require_registered(name)?;
    let data = crate::config::plugin_data_path(name).map_err(|error| error.to_string())?;
    let plugin = load_agent_plugin(&registered.root, &data).map_err(|error| error.to_string())?;
    let mut lines = vec![
        format!("name: {}", plugin.name),
        format!("root: {}", plugin.root.display()),
        format!("enabled: {}", registered.enabled),
    ];
    if let Some(version) = &plugin.version {
        lines.push(format!("version: {version}"));
    }
    for server in &plugin.mcp_servers {
        lines.push(format!("server: {}", server.server_name));
    }
    for skill in &plugin.skills.skills {
        lines.push(format!("skill: {}", skill.name));
    }
    for hook in &plugin.hooks {
        lines.push(format!("hook: {} #{}", hook.event.as_str(), hook.index + 1));
    }
    lines.extend(warning_lines(&plugin));
    Ok(lines)
}

pub(crate) fn enable(name: &str) -> Result<Vec<String>, String> {
    let registered = require_registered(name)?;
    let data = crate::config::plugin_data_path(name).map_err(|error| error.to_string())?;
    let plugin = load_agent_plugin(&registered.root, &data).map_err(|error| error.to_string())?;
    if plugin.name != name {
        return Err(format!(
            "registered plugin {name} now declares name {}",
            plugin.name
        ));
    }
    let mut lines = warning_lines(&plugin);
    crate::config::enable_plugin(name, &plugin.root)
        .map_err(|error| format!("cannot enable plugin: {error}"))?;
    lines.push(format!("enabled plugin {name}; applies on next launch"));
    Ok(lines)
}

pub(crate) fn disable(name: &str) -> Result<Vec<String>, String> {
    require_registered(name)?;
    crate::config::set_plugin_enabled(name, false)
        .map_err(|error| format!("cannot disable plugin: {error}"))?;
    Ok(vec![format!(
        "disabled plugin {name}; applies on next launch"
    )])
}

pub(crate) fn uninstall(name: &str) -> Result<Vec<String>, String> {
    require_registered(name)?;
    crate::config::remove_plugin(name)
        .map_err(|error| format!("cannot uninstall plugin: {error}"))?;
    let data = crate::config::plugin_data_path(name).map_err(|error| error.to_string())?;
    Ok(vec![format!(
        "uninstalled plugin {name}; retained plugin data at {}",
        data.display()
    )])
}

fn require_registered(name: &str) -> Result<crate::config::RegisteredPlugin, String> {
    crate::config::stored_plugin(name).ok_or_else(|| format!("plugin is not installed: {name}"))
}
