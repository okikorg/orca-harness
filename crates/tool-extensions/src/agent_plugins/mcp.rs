use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{Map, Value};

use crate::mcp::{ProcessEnvironment, StdioLaunch};

use super::paths::resolve_descendant;
use super::{normalize_plugin_server_id, PluginMcpServer, PluginWarning, MCP_SCHEMA};

const STDIO_FIELDS: &[&str] = &["type", "command", "args", "env", "cwd"];
const REMOTE_FIELDS: &[&str] = &["type", "url", "headers"];

pub(super) fn load(
    path: &Path,
    root: &Path,
    plugin_data: &Path,
    plugin_name: &str,
    warnings: &mut Vec<PluginWarning>,
) -> Vec<PluginMcpServer> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => return invalid_document(warnings, format!("cannot read mcp.json: {error}")),
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => return invalid_document(warnings, format!("invalid JSON: {error}")),
    };
    let object = match value.as_object() {
        Some(object) => object,
        None => return invalid_document(warnings, "document must be an object"),
    };
    let servers = match validate_document(object) {
        Ok(servers) => servers,
        Err(message) => return invalid_document(warnings, message),
    };

    let mut parsed = Vec::new();
    for (server_name, value) in servers {
        match parse_server(value, root, plugin_data, plugin_name, server_name) {
            Ok(Some(server)) => parsed.push(server),
            Ok(None) => warnings.push(PluginWarning::new(
                format!("mcpServers.{server_name}"),
                "transport is valid but unsupported by Orcacode v1",
            )),
            Err(message) => warnings.push(PluginWarning::new(
                format!("mcpServers.{server_name}"),
                message,
            )),
        }
    }
    remove_collisions(parsed, warnings)
}

fn invalid_document(
    warnings: &mut Vec<PluginWarning>,
    message: impl Into<String>,
) -> Vec<PluginMcpServer> {
    warnings.push(PluginWarning::new("mcp.json", message));
    Vec::new()
}

fn validate_document(object: &Map<String, Value>) -> Result<&Map<String, Value>, String> {
    if object.len() != 2 || !object.contains_key("$schema") || !object.contains_key("mcpServers") {
        return Err("mcp.json must contain only required $schema and mcpServers fields".into());
    }
    match object.get("$schema").and_then(Value::as_str) {
        Some(MCP_SCHEMA) => {}
        Some(schema) => return Err(format!("unsupported $schema: {schema}")),
        None => return Err("$schema must be a string".into()),
    }
    object
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or_else(|| "mcpServers must be an object".into())
}

fn parse_server(
    value: &Value,
    root: &Path,
    plugin_data: &Path,
    plugin_name: &str,
    server_name: &str,
) -> Result<Option<PluginMcpServer>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "server entry must be an object".to_string())?;
    let transport = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "type must be a string".to_string())?;
    match transport {
        "stdio" => parse_stdio(object, root, plugin_data, plugin_name, server_name).map(Some),
        "streamable-http" | "sse" => {
            validate_remote(object)?;
            Ok(None)
        }
        other => Err(format!("unsupported or unknown server type {other}")),
    }
}

fn parse_stdio(
    object: &Map<String, Value>,
    root: &Path,
    plugin_data: &Path,
    plugin_name: &str,
    server_name: &str,
) -> Result<PluginMcpServer, String> {
    reject_unknown_fields(object, STDIO_FIELDS)?;
    let root_text = utf8_path(root, "plugin root")?;
    let plugin_data_text = utf8_path(plugin_data, "plugin data path")?;
    let command = object
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| "command must be a non-empty string".to_string())?;
    let command = resolve_command(command, root)?;
    let args = parse_args(object.get("args"))?
        .into_iter()
        .map(|value| expand(&value, root_text, plugin_data_text))
        .collect();
    let mut env = parse_env(object.get("env"))?
        .into_iter()
        .map(|(name, value)| (name, expand(&value, root_text, plugin_data_text)))
        .collect::<BTreeMap<_, _>>();
    env.insert("PLUGIN_ROOT".into(), root_text.to_owned());
    env.insert("PLUGIN_DATA".into(), plugin_data_text.to_owned());
    let cwd = parse_cwd(object.get("cwd"), root, plugin_data)?;

    Ok(PluginMcpServer {
        id: normalize_plugin_server_id(plugin_name, server_name),
        plugin_name: plugin_name.to_owned(),
        server_name: server_name.to_owned(),
        launch: StdioLaunch {
            command,
            args,
            env,
            cwd: Some(cwd),
            environment: ProcessEnvironment::Sanitized,
        },
    })
}

fn resolve_command(command: &str, root: &Path) -> Result<String, String> {
    if command.is_empty() || command.contains('\0') || command.chars().any(char::is_whitespace) {
        return Err("command must be one non-empty executable token".into());
    }
    if let Some(suffix) = command.strip_prefix("./") {
        return resolve_descendant(root, Path::new(suffix))
            .and_then(|path| {
                path.into_os_string()
                    .into_string()
                    .map_err(|_| "command resolves to a non-UTF-8 path".to_string())
            })
            .map_err(|message| format!("invalid plugin-relative command: {message}"));
    }
    if command.contains('/') || command.contains('\\') || Path::new(command).is_absolute() {
        return Err("command must be a bare executable name or begin with ./".into());
    }
    Ok(command.to_owned())
}

fn parse_args(value: Option<&Value>) -> Result<Vec<String>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) if values.iter().all(Value::is_string) => Ok(values
            .iter()
            .map(|value| value.as_str().expect("checked string").to_owned())
            .collect()),
        Some(_) => Err("args must be an array of strings".into()),
    }
}

fn parse_env(value: Option<&Value>) -> Result<BTreeMap<String, String>, String> {
    parse_env_with_semantics(value, cfg!(windows))
}

fn parse_env_with_semantics(
    value: Option<&Value>,
    case_insensitive_names: bool,
) -> Result<BTreeMap<String, String>, String> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let object = value
        .as_object()
        .ok_or_else(|| "env must be an object of strings".to_string())?;
    let mut env = BTreeMap::new();
    for (name, value) in object {
        if is_reserved_env_name(name, case_insensitive_names) {
            return Err(format!("env must not override reserved {name}"));
        }
        let value = value
            .as_str()
            .ok_or_else(|| format!("env value {name} must be a string"))?;
        env.insert(name.clone(), value.to_owned());
    }
    Ok(env)
}

fn parse_cwd(value: Option<&Value>, root: &Path, plugin_data: &Path) -> Result<PathBuf, String> {
    let Some(value) = value else {
        return Ok(root.to_owned());
    };
    let value = value
        .as_str()
        .ok_or_else(|| "cwd must be a string".to_string())?;
    let boundary = if value.starts_with("./") || is_placeholder_rooted(value, "${PLUGIN_ROOT}") {
        root
    } else if is_placeholder_rooted(value, "${PLUGIN_DATA}") {
        plugin_data
    } else {
        return Err("cwd must begin with ./, ${PLUGIN_ROOT}, or ${PLUGIN_DATA}".into());
    };
    let expanded = expand(
        value,
        utf8_path(root, "plugin root")?,
        utf8_path(plugin_data, "plugin data path")?,
    );
    resolve_descendant(boundary, Path::new(&expanded))
}

fn is_placeholder_rooted(value: &str, placeholder: &str) -> bool {
    value == placeholder
        || value
            .strip_prefix(placeholder)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn expand(value: &str, root: &str, plugin_data: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while cursor < value.len() {
        let remaining = &value[cursor..];
        if remaining.starts_with("${PLUGIN_ROOT}") {
            output.push_str(root);
            cursor += "${PLUGIN_ROOT}".len();
        } else if remaining.starts_with("${PLUGIN_DATA}") {
            output.push_str(plugin_data);
            cursor += "${PLUGIN_DATA}".len();
        } else {
            let character = remaining.chars().next().expect("cursor within string");
            output.push(character);
            cursor += character.len_utf8();
        }
    }
    output
}

fn utf8_path<'a>(path: &'a Path, label: &str) -> Result<&'a str, String> {
    path.to_str()
        .ok_or_else(|| format!("{label} must be valid UTF-8"))
}

fn is_reserved_env_name(name: &str, case_insensitive: bool) -> bool {
    ["PLUGIN_ROOT", "PLUGIN_DATA"].iter().any(|reserved| {
        if case_insensitive {
            name.eq_ignore_ascii_case(reserved)
        } else {
            name == *reserved
        }
    })
}

fn validate_remote(object: &Map<String, Value>) -> Result<(), String> {
    reject_unknown_fields(object, REMOTE_FIELDS)?;
    let url = object
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| "url must be a non-empty string".to_string())?;
    validate_remote_url(url)?;
    if let Some(headers) = object.get("headers") {
        let headers = headers
            .as_object()
            .ok_or_else(|| "headers must be an object of strings".to_string())?;
        let mut names = std::collections::BTreeSet::new();
        for (name, value) in headers {
            HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("invalid HTTP header name {name}"))?;
            let value = value
                .as_str()
                .ok_or_else(|| format!("HTTP header {name} must have a string value"))?;
            HeaderValue::from_str(value)
                .map_err(|_| format!("invalid value for HTTP header {name}"))?;
            if !names.insert(name.to_ascii_lowercase()) {
                return Err(format!("duplicate case-insensitive HTTP header {name}"));
            }
        }
    }
    Ok(())
}

fn validate_remote_url(value: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(value).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("URL must be an absolute HTTP or HTTPS URL".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL must not contain user information".into());
    }
    if url.fragment().is_some() {
        return Err("URL must not contain a fragment".into());
    }
    if url.scheme() == "http" && !url.host_str().is_some_and(is_loopback_host) {
        return Err("non-loopback MCP URLs must use HTTPS".into());
    }
    Ok(())
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn reject_unknown_fields(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(format!("unknown field {field}"));
    }
    Ok(())
}

fn remove_collisions(
    servers: Vec<PluginMcpServer>,
    warnings: &mut Vec<PluginWarning>,
) -> Vec<PluginMcpServer> {
    let mut counts = HashMap::<String, usize>::new();
    for server in &servers {
        *counts.entry(server.id.clone()).or_default() += 1;
    }
    let mut kept = Vec::new();
    for server in servers {
        if counts[&server.id] > 1 {
            warnings.push(PluginWarning::new(
                format!("mcpServers.{}", server.server_name),
                format!("normalized server ID collision: {}", server.id),
            ));
        } else {
            kept.push(server);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::parse_env_with_semantics;

    #[test]
    fn agent_plugin_reserved_env_names_follow_platform_semantics() {
        let aliases = json!({ "plugin_root": "wrong", "Plugin_Data": "wrong" });
        let portable = json!({ "PLUGIN_CACHE": "allowed" });

        assert!(parse_env_with_semantics(Some(&aliases), true).is_err());
        assert!(parse_env_with_semantics(Some(&aliases), false).is_ok());
        assert!(parse_env_with_semantics(Some(&portable), true).is_ok());
    }
}
