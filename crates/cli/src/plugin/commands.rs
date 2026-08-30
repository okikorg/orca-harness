use std::path::Path;

use orca_harness_core::Tool;
use orca_harness_tool_extensions::agent_plugins::{load_agent_plugin, AgentPlugin};
use orca_harness_tool_extensions::mcp::McpClient;

use super::PluginCommand;

pub(super) async fn run(command: PluginCommand) -> Result<(), String> {
    match command {
        PluginCommand::Init { name, language } => {
            let target = super::scaffold::create(&name, language)?;
            println!("created plugin scaffold at {}", target.display());
        }
        PluginCommand::Validate { path } => validate(&path)?,
        PluginCommand::Test { path } => test(&path).await?,
        PluginCommand::Install { path } => install(&path)?,
        PluginCommand::List => list(),
        PluginCommand::Inspect { name } => inspect(&name)?,
        PluginCommand::Enable { name } => enable(&name)?,
        PluginCommand::Disable { name } => disable(&name)?,
        PluginCommand::Uninstall { name } => uninstall(&name)?,
    }
    Ok(())
}

fn validation_data_path() -> Result<std::path::PathBuf, String> {
    crate::config::plugin_data_path("validation-probe").map_err(|error| error.to_string())
}

fn load_for_validation(path: &Path) -> Result<AgentPlugin, String> {
    load_agent_plugin(path, &validation_data_path()?).map_err(|error| error.to_string())
}

fn print_warnings(plugin: &AgentPlugin) {
    for warning in &plugin.warnings {
        println!("warning: {warning}");
    }
}

fn validate(path: &Path) -> Result<(), String> {
    let plugin = load_for_validation(path)?;
    print_warnings(&plugin);
    println!("valid plugin: {} ({})", plugin.name, plugin.root.display());
    Ok(())
}

async fn test(path: &Path) -> Result<(), String> {
    let initial = load_for_validation(path)?;
    let data = crate::config::plugin_data_path(&initial.name).map_err(|error| error.to_string())?;
    let plugin = load_agent_plugin(&initial.root, &data).map_err(|error| error.to_string())?;
    print_warnings(&plugin);
    if !plugin.mcp_servers.is_empty() {
        crate::config::create_plugin_data_dir(&data)
            .map_err(|error| format!("cannot create plugin data directory: {error}"))?;
    }
    let mut failures = Vec::new();
    for server in &plugin.mcp_servers {
        match McpClient::connect_stdio(&server.id, &server.launch).await {
            Ok(connection) => {
                println!("server {}:", server.server_name);
                for tool in connection.tools() {
                    println!("  {}", tool.schema().name);
                }
                drop(connection);
            }
            Err(error) => failures.push(format!("{}: {error}", server.server_name)),
        }
    }
    if !failures.is_empty() {
        return Err(format!(
            "plugin server test failed ({}); install dependencies and build required output first",
            failures.join("; ")
        ));
    }
    println!("plugin test passed: {}", plugin.name);
    Ok(())
}

fn install(path: &Path) -> Result<(), String> {
    let plugin = load_for_validation(path)?;
    print_warnings(&plugin);
    if let Some(existing) = crate::config::stored_plugin(&plugin.name) {
        if existing.root == plugin.root {
            println!("plugin already installed: {}", plugin.name);
            return Ok(());
        }
        return Err(format!(
            "plugin {} is already registered from a different root: {}",
            plugin.name,
            existing.root.display()
        ));
    }
    crate::config::save_plugin(&plugin.name, &plugin.root, false)
        .map_err(|error| format!("cannot save plugin registration: {error}"))?;
    println!("installed plugin {} (disabled)", plugin.name);
    Ok(())
}

fn list() {
    for plugin in crate::config::stored_plugins() {
        let state = if plugin.enabled {
            "enabled"
        } else {
            "disabled"
        };
        println!("{}\t{}\t{}", plugin.name, state, plugin.root.display());
    }
}

fn inspect(name: &str) -> Result<(), String> {
    let registered = require_registered(name)?;
    let data = crate::config::plugin_data_path(name).map_err(|error| error.to_string())?;
    let plugin = load_agent_plugin(&registered.root, &data).map_err(|error| error.to_string())?;
    println!("name: {}", plugin.name);
    println!("root: {}", plugin.root.display());
    println!("enabled: {}", registered.enabled);
    if let Some(version) = &plugin.version {
        println!("version: {version}");
    }
    for server in &plugin.mcp_servers {
        println!("server: {}", server.server_name);
    }
    print_warnings(&plugin);
    Ok(())
}

fn enable(name: &str) -> Result<(), String> {
    let registered = require_registered(name)?;
    let data = crate::config::plugin_data_path(name).map_err(|error| error.to_string())?;
    let plugin = load_agent_plugin(&registered.root, &data).map_err(|error| error.to_string())?;
    if plugin.name != name {
        return Err(format!(
            "registered plugin {name} now declares name {}",
            plugin.name
        ));
    }
    print_warnings(&plugin);
    crate::config::enable_plugin(name, &plugin.root)
        .map_err(|error| format!("cannot enable plugin: {error}"))?;
    println!("enabled plugin {name}; applies on next launch");
    Ok(())
}

fn disable(name: &str) -> Result<(), String> {
    require_registered(name)?;
    crate::config::set_plugin_enabled(name, false)
        .map_err(|error| format!("cannot disable plugin: {error}"))?;
    println!("disabled plugin {name}; applies on next launch");
    Ok(())
}

fn uninstall(name: &str) -> Result<(), String> {
    require_registered(name)?;
    crate::config::remove_plugin(name)
        .map_err(|error| format!("cannot uninstall plugin: {error}"))?;
    let data = crate::config::plugin_data_path(name).map_err(|error| error.to_string())?;
    println!(
        "uninstalled plugin {name}; retained plugin data at {}",
        data.display()
    );
    Ok(())
}

fn require_registered(name: &str) -> Result<crate::config::RegisteredPlugin, String> {
    crate::config::stored_plugin(name).ok_or_else(|| format!("plugin is not installed: {name}"))
}
