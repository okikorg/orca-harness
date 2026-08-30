//! Static loading and validation for Agent Plugins 1.0 packages.
//!
//! Loading only reads package metadata. It does not create plugin data,
//! install dependencies, or execute plugin-provided code.

mod manifest;
mod mcp;
mod paths;

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::mcp::StdioLaunch;

pub(crate) const PLUGIN_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
pub(crate) const MCP_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json";

#[derive(Debug)]
pub struct AgentPlugin {
    pub name: String,
    pub version: Option<String>,
    pub root: PathBuf,
    pub mcp_servers: Vec<PluginMcpServer>,
    pub warnings: Vec<PluginWarning>,
}

#[derive(Debug)]
pub struct PluginMcpServer {
    pub id: String,
    pub plugin_name: String,
    pub server_name: String,
    pub launch: StdioLaunch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginWarning {
    pub scope: String,
    pub message: String,
}

impl PluginWarning {
    pub(crate) fn new(scope: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            scope: scope.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for PluginWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.scope, self.message)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PluginError {
    message: String,
}

impl PluginError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Load and statically validate one Agent Plugins 1.0 package.
pub fn load_agent_plugin(root: &Path, plugin_data: &Path) -> Result<AgentPlugin, PluginError> {
    let root = paths::canonical_directory(root, "plugin root")?;
    let plugin_data = paths::normalize_data_boundary(plugin_data)?;
    let manifest_path = paths::required_file(&root, "plugin.json")?;
    let bytes = fs::read(&manifest_path)
        .map_err(|error| PluginError::new(format!("cannot read plugin.json: {error}")))?;
    let parsed = manifest::parse(&bytes)?;
    let mut warnings = parsed.warnings;

    warn_for_unsupported_components(&root, &parsed.extension_names, &mut warnings);
    let mcp_servers = match paths::optional_file(&root, "mcp.json") {
        Ok(Some(path)) => mcp::load(&path, &root, &plugin_data, &parsed.name, &mut warnings),
        Ok(None) => Vec::new(),
        Err(message) => {
            warnings.push(PluginWarning::new("mcp.json", message));
            Vec::new()
        }
    };

    Ok(AgentPlugin {
        name: parsed.name,
        version: parsed.version,
        root,
        mcp_servers,
        warnings,
    })
}

/// Convert plugin and server names into the host-reserved MCP namespace.
pub fn normalize_plugin_server_id(plugin_name: &str, server_name: &str) -> String {
    format!(
        "plugin__{}__{}",
        normalize_id_part(plugin_name),
        normalize_id_part(server_name)
    )
}

fn normalize_id_part(value: &str) -> String {
    let mut normalized = String::new();
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !normalized.is_empty() {
                normalized.push('_');
            }
            normalized.push(character.to_ascii_lowercase());
            separator = false;
        } else {
            separator = true;
        }
    }
    if normalized.is_empty() {
        normalized.push_str("unnamed");
    }
    normalized
}

fn warn_for_unsupported_components(
    root: &Path,
    manifest_namespaces: &[String],
    warnings: &mut Vec<PluginWarning>,
) {
    if path_is_present(&root.join("skills")) {
        warnings.push(PluginWarning::new(
            "skills",
            "Orcacode v1 does not load Agent Skills",
        ));
    }

    let mut namespaces: BTreeSet<String> = manifest_namespaces.iter().cloned().collect();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if looks_like_extension_namespace(&name)
                && fs::metadata(entry.path()).is_ok_and(|metadata| metadata.is_dir())
            {
                namespaces.insert(name);
            }
        }
    }
    warnings.extend(namespaces.into_iter().map(|namespace| {
        PluginWarning::new(
            format!("extensions.{namespace}"),
            "Orcacode v1 does not load this client extension namespace",
        )
    }));
}

fn path_is_present(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

fn looks_like_extension_namespace(name: &str) -> bool {
    let parts: Vec<_> = name.split('.').collect();
    parts.len() >= 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && part
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && part
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
}
