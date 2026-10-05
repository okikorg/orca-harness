//! Static parsing of OpenAI Agents API (`.codex-plugin`) plugin archives.
//!
//! Hosts that stage a plugin into a sandbox hold the archive in memory and
//! run its servers somewhere other than the host process. This parser
//! therefore never touches the filesystem and never spawns anything: it
//! reads archive entries and returns launch specifications whose paths are
//! resolved against the directory the host will stage the plugin into.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::skills::parse_frontmatter;

use super::{normalize_plugin_server_id, PluginError, PluginWarning};

const MANIFEST: &str = ".codex-plugin/plugin.json";
const DEFAULT_MCP: &str = ".mcp.json";
const DEFAULT_SKILLS: &str = "skills";
const KNOWN_MANIFEST_FIELDS: &[&str] = &[
    "$schema",
    "name",
    "version",
    "description",
    "skills",
    "mcpServers",
    "author",
    "homepage",
    "repository",
    "license",
    "keywords",
    "interface",
];
const STDIO_FIELDS: &[&str] = &[
    "type",
    "command",
    "args",
    "cwd",
    "env_vars",
    "enabled",
    "enabled_tools",
    "disabled_tools",
];
const HTTP_FIELDS: &[&str] = &[
    "type",
    "url",
    "bearer_token_env_var",
    "http_headers",
    "enabled",
    "enabled_tools",
    "disabled_tools",
];

/// One plugin parsed from an archive.
#[derive(Debug)]
pub struct CodexPlugin {
    pub name: String,
    pub version: Option<String>,
    pub description: String,
    /// Archive prefix of the plugin root: empty when the manifest sits at
    /// the archive root, otherwise the single wrapping folder plus `/`.
    /// Hosts strip it before staging so `root` is the plugin root.
    pub prefix: String,
    pub skills: Vec<CodexPluginSkill>,
    pub mcp_servers: Vec<CodexPluginServer>,
    pub warnings: Vec<PluginWarning>,
}

/// A skill directory inside the plugin, relative to the plugin root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexPluginSkill {
    pub name: String,
    pub description: String,
    pub dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexPluginServer {
    /// Host-reserved identifier, `plugin__<plugin>__<server>`.
    pub id: String,
    pub server_name: String,
    pub transport: CodexPluginTransport,
    /// Only these remote tool names are exposed, when set.
    pub enabled_tools: Option<Vec<String>>,
    /// These remote tool names are never exposed.
    pub disabled_tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexPluginTransport {
    /// `command` is a bare executable name, an absolute path, or resolved
    /// against the staged plugin root; `cwd` is always absolute.
    Stdio {
        command: String,
        args: Vec<String>,
        cwd: String,
        env_vars: Vec<String>,
    },
    Http {
        url: String,
        bearer_token_env_var: Option<String>,
        headers: BTreeMap<String, String>,
    },
}

/// Parse one plugin from archive `entries` (path, contents), resolving
/// server paths against `root`, the absolute directory the host stages the
/// plugin root into.
pub fn parse_codex_plugin(
    entries: &[(&str, &[u8])],
    root: &str,
) -> Result<CodexPlugin, PluginError> {
    if !root.starts_with('/') || root.len() > 1 && root.ends_with('/') {
        return Err(PluginError::new(
            "plugin root must be an absolute path without a trailing slash",
        ));
    }
    for (path, _) in entries {
        relative_path(path)
            .map_err(|message| PluginError::new(format!("archive entry {path:?}: {message}")))?;
    }
    let prefix = plugin_prefix(entries)?;
    let files: BTreeMap<&str, &[u8]> = entries
        .iter()
        .filter_map(|(path, bytes)| path.strip_prefix(prefix.as_str()).map(|p| (p, *bytes)))
        .collect();
    let mut warnings = Vec::new();

    let manifest: Value = serde_json::from_slice(files[MANIFEST])
        .map_err(|error| PluginError::new(format!("{MANIFEST} is not valid JSON: {error}")))?;
    let manifest = manifest
        .as_object()
        .ok_or_else(|| PluginError::new(format!("{MANIFEST} must be an object")))?;
    let name = required_string(manifest, "name")?;
    validate_name(name)?;
    let description = required_string(manifest, "description")?.to_owned();
    let version = match manifest.get("version") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(_) => return Err(PluginError::new("plugin.json version must be a string")),
    };
    for key in manifest.keys() {
        if matches!(key.as_str(), "hooks" | "apps") {
            warnings.push(PluginWarning::new(
                key.as_str(),
                "not supported by this host; ignored",
            ));
        } else if !KNOWN_MANIFEST_FIELDS.contains(&key.as_str()) {
            warnings.push(PluginWarning::new(
                key.as_str(),
                "unknown manifest field ignored",
            ));
        }
    }

    let skills_dir = component(manifest, "skills", DEFAULT_SKILLS)?;
    let skills = skills(&files, &skills_dir, &mut warnings);

    let mcp_file = component(manifest, "mcpServers", DEFAULT_MCP)?;
    let mcp_servers = match files.get(mcp_file.as_str()) {
        Some(bytes) => mcp_servers(bytes, name, root, &mut warnings),
        None if manifest.contains_key("mcpServers") => {
            warnings.push(PluginWarning::new(
                "mcpServers",
                format!("{mcp_file} is not in the archive"),
            ));
            Vec::new()
        }
        None => Vec::new(),
    };

    Ok(CodexPlugin {
        name: name.to_owned(),
        version,
        description,
        prefix,
        skills,
        mcp_servers,
        warnings,
    })
}

/// The manifest may sit at the archive root, or inside exactly one folder
/// that wraps every entry (the documented upload layout).
fn plugin_prefix(entries: &[(&str, &[u8])]) -> Result<String, PluginError> {
    if entries.iter().any(|(path, _)| *path == MANIFEST) {
        return Ok(String::new());
    }
    let tops: BTreeSet<&str> = entries
        .iter()
        .map(|(path, _)| path.split('/').next().unwrap_or(path))
        .collect();
    if let [top] = tops.into_iter().collect::<Vec<_>>()[..] {
        let prefix = format!("{top}/");
        let manifest = format!("{prefix}{MANIFEST}");
        if entries.iter().any(|(path, _)| *path == manifest) {
            return Ok(prefix);
        }
    }
    Err(PluginError::new(format!(
        "archive must contain {MANIFEST} at its root or inside one plugin folder"
    )))
}

fn relative_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("must be a normalized relative path");
    }
    Ok(())
}

/// A manifest path field: `./`-prefixed, inside the plugin, normalized.
fn component(
    manifest: &Map<String, Value>,
    field: &str,
    default: &str,
) -> Result<String, PluginError> {
    let Some(value) = manifest.get(field) else {
        return Ok(default.to_owned());
    };
    let raw = value
        .as_str()
        .ok_or_else(|| PluginError::new(format!("plugin.json {field} must be a string")))?;
    let inner = raw
        .strip_prefix("./")
        .ok_or_else(|| PluginError::new(format!("plugin.json {field} must start with ./")))?
        .trim_end_matches('/');
    relative_path(inner)
        .map_err(|message| PluginError::new(format!("plugin.json {field}: {message}")))?;
    Ok(inner.to_owned())
}

fn skills(
    files: &BTreeMap<&str, &[u8]>,
    skills_dir: &str,
    warnings: &mut Vec<PluginWarning>,
) -> Vec<CodexPluginSkill> {
    let prefix = format!("{skills_dir}/");
    let mut found = Vec::new();
    let mut names = BTreeSet::new();
    for (path, bytes) in files {
        let Some(rest) = path.strip_prefix(prefix.as_str()) else {
            continue;
        };
        let Some((dir_name, "SKILL.md")) = rest.split_once('/') else {
            continue;
        };
        let scope = format!("skills.{dir_name}");
        let front = match std::str::from_utf8(bytes)
            .map_err(|_| "SKILL.md must be UTF-8".to_owned())
            .and_then(parse_frontmatter)
        {
            Ok(front) => front,
            Err(message) => {
                warnings.push(PluginWarning::new(scope, message));
                continue;
            }
        };
        let name = front.name.unwrap_or_else(|| dir_name.to_owned());
        let Some(description) = front.description.filter(|value| !value.is_empty()) else {
            warnings.push(PluginWarning::new(
                scope,
                "no description in the frontmatter",
            ));
            continue;
        };
        if !names.insert(name.clone()) {
            warnings.push(PluginWarning::new(
                scope,
                format!("duplicate skill name {name}"),
            ));
            continue;
        }
        found.push(CodexPluginSkill {
            name,
            description,
            dir: format!("{skills_dir}/{dir_name}"),
        });
    }
    found
}

fn mcp_servers(
    bytes: &[u8],
    plugin_name: &str,
    root: &str,
    warnings: &mut Vec<PluginWarning>,
) -> Vec<CodexPluginServer> {
    let document: Value = match serde_json::from_slice(bytes) {
        Ok(document) => document,
        Err(error) => {
            warnings.push(PluginWarning::new(
                "mcpServers",
                format!("invalid JSON: {error}"),
            ));
            return Vec::new();
        }
    };
    let Some(servers) = document.get("mcpServers").and_then(Value::as_object) else {
        warnings.push(PluginWarning::new(
            "mcpServers",
            "mcpServers must be an object",
        ));
        return Vec::new();
    };
    let mut ids = BTreeSet::new();
    let mut parsed = Vec::new();
    for (server_name, spec) in servers {
        let scope = format!("mcpServers.{server_name}");
        match server(spec, plugin_name, server_name, root) {
            Ok(None) => {}
            Ok(Some((server, ignored))) => {
                for field in ignored {
                    warnings.push(PluginWarning::new(
                        scope.clone(),
                        format!("field {field} is not supported; ignored"),
                    ));
                }
                if ids.insert(server.id.clone()) {
                    parsed.push(server);
                } else {
                    warnings.push(PluginWarning::new(scope, "duplicate normalized server id"));
                }
            }
            Err(message) => warnings.push(PluginWarning::new(scope, message)),
        }
    }
    parsed
}

type Parsed = Option<(CodexPluginServer, Vec<String>)>;

fn server(
    spec: &Value,
    plugin_name: &str,
    server_name: &str,
    root: &str,
) -> Result<Parsed, String> {
    let object = spec.as_object().ok_or("server entry must be an object")?;
    if object.get("enabled") == Some(&Value::Bool(false)) {
        return Ok(None);
    }
    let kind = match object.get("type") {
        Some(Value::String(kind)) => kind.as_str(),
        Some(_) => return Err("type must be a string".into()),
        None if object.contains_key("command") => "stdio",
        None if object.contains_key("url") => "http",
        None => return Err("server needs a command or a url".into()),
    };
    let (transport, fields) = match kind {
        "stdio" => (stdio(object, root)?, STDIO_FIELDS),
        "http" | "streamable_http" => (http(object)?, HTTP_FIELDS),
        other => return Err(format!("unsupported server type {other}")),
    };
    let ignored = object
        .keys()
        .filter(|key| !fields.contains(&key.as_str()))
        .cloned()
        .collect();
    Ok(Some((
        CodexPluginServer {
            id: normalize_plugin_server_id(plugin_name, server_name),
            server_name: server_name.to_owned(),
            transport,
            enabled_tools: optional_strings(object, "enabled_tools")?,
            disabled_tools: optional_strings(object, "disabled_tools")?.unwrap_or_default(),
        },
        ignored,
    )))
}

fn stdio(object: &Map<String, Value>, root: &str) -> Result<CodexPluginTransport, String> {
    let command = object
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.trim().is_empty())
        .ok_or("command must be a non-empty string")?;
    let command = match command.strip_prefix("./") {
        Some(inner) => {
            relative_path(inner).map_err(|message| format!("command {message}"))?;
            format!("{root}/{inner}")
        }
        None => command.to_owned(),
    };
    let cwd = match object.get("cwd") {
        None | Some(Value::Null) => root.to_owned(),
        Some(Value::String(cwd)) if cwd.starts_with('/') => {
            return Err("cwd must be relative to the plugin root".into())
        }
        Some(Value::String(cwd)) => {
            let inner = cwd.strip_prefix("./").unwrap_or(cwd).trim_end_matches('/');
            if inner.is_empty() || inner == "." {
                root.to_owned()
            } else {
                relative_path(inner).map_err(|message| format!("cwd {message}"))?;
                format!("{root}/{inner}")
            }
        }
        Some(_) => return Err("cwd must be a string".into()),
    };
    let env_vars = optional_strings(object, "env_vars")?.unwrap_or_default();
    if let Some(bad) = env_vars.iter().find(|name| !env_name(name)) {
        return Err(format!("invalid env_vars entry {bad:?}"));
    }
    Ok(CodexPluginTransport::Stdio {
        command,
        args: optional_strings(object, "args")?.unwrap_or_default(),
        cwd,
        env_vars,
    })
}

fn http(object: &Map<String, Value>) -> Result<CodexPluginTransport, String> {
    let url = object
        .get("url")
        .and_then(Value::as_str)
        .ok_or("url must be a string")?;
    let (scheme, rest) = url.split_once("://").ok_or("url must be absolute")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if !matches!(scheme, "http" | "https")
        || host.is_empty()
        || host.contains('@')
        || url.contains('#')
    {
        return Err("url must be an http(s) URL without userinfo or fragment".into());
    }
    let bearer_token_env_var = match object.get("bearer_token_env_var") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) if env_name(name) => Some(name.clone()),
        Some(_) => return Err("bearer_token_env_var must be an environment variable name".into()),
    };
    let mut headers = BTreeMap::new();
    if let Some(value) = object.get("http_headers") {
        let map = value.as_object().ok_or("http_headers must be an object")?;
        for (key, value) in map {
            let value = value
                .as_str()
                .ok_or("http_headers values must be strings")?;
            headers.insert(key.clone(), value.to_owned());
        }
    }
    Ok(CodexPluginTransport::Http {
        url: url.to_owned(),
        bearer_token_env_var,
        headers,
    })
}

fn optional_strings(
    object: &Map<String, Value>,
    field: &str,
) -> Result<Option<Vec<String>>, String> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{field} must be an array of strings"))
            })
            .collect::<Result<_, _>>()
            .map(Some),
        Some(_) => Err(format!("{field} must be an array of strings")),
    }
}

fn env_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && !bytes[0].is_ascii_digit()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, PluginError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::new(format!("plugin.json {field} must be a non-empty string")))
}

/// Uploaded plugin names are path segments and tool-name parts: letters,
/// digits, `-`, `_` and `.`, never `..`.
fn validate_name(name: &str) -> Result<(), PluginError> {
    let valid = (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && !name.contains("..");
    if !valid {
        return Err(PluginError::new(
            "plugin.json name must be 1-64 ASCII letters, digits, -, _ or ., start alphanumeric, and not contain ..",
        ));
    }
    Ok(())
}
