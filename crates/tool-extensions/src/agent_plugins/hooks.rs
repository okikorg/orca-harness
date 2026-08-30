use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::mcp::{ProcessEnvironment, StdioLaunch};

use super::mcp::{
    expand, parse_args, parse_cwd, parse_env, reject_unknown_fields, resolve_command, utf8_path,
};
use super::{PluginHook, PluginHookEvent, PluginWarning, ORCACODE_EXTENSION_NAMESPACE};

const DOCUMENT_FIELDS: &[&str] = &["version", "hooks"];
const HOOK_FIELDS: &[&str] = &["command", "args", "env", "cwd", "timeout_ms"];
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 30_000;

pub(super) fn load(
    root: &Path,
    plugin_data: &Path,
    plugin_name: &str,
    warnings: &mut Vec<PluginWarning>,
) -> Vec<PluginHook> {
    let relative = format!("{ORCACODE_EXTENSION_NAMESPACE}/hooks.json");
    let path = match super::paths::optional_file(root, &relative) {
        Ok(Some(path)) => path,
        Ok(None) => return Vec::new(),
        Err(message) => return invalid_document(warnings, message),
    };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return invalid_document(warnings, format!("cannot read hooks.json: {error}"))
        }
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => return invalid_document(warnings, format!("invalid JSON: {error}")),
    };
    let object = match value.as_object() {
        Some(object) => object,
        None => return invalid_document(warnings, "document must be an object"),
    };
    if let Err(message) = validate_document(object) {
        return invalid_document(warnings, message);
    }
    let events = object["hooks"].as_object().expect("validated hooks object");
    let mut hooks = Vec::new();
    for (event_name, entries) in events {
        let Some(event) = parse_event(event_name) else {
            warnings.push(PluginWarning::new(
                hook_scope(event_name, None),
                "unsupported hook event",
            ));
            continue;
        };
        let Some(entries) = entries.as_array() else {
            warnings.push(PluginWarning::new(
                hook_scope(event_name, None),
                "hook event must be an array",
            ));
            continue;
        };
        for (index, entry) in entries.iter().enumerate() {
            match parse_hook(entry, root, plugin_data, plugin_name, event, index) {
                Ok(hook) => hooks.push(hook),
                Err(message) => warnings.push(PluginWarning::new(
                    hook_scope(event_name, Some(index)),
                    message,
                )),
            }
        }
    }
    hooks
}

fn validate_document(object: &Map<String, Value>) -> Result<(), String> {
    reject_unknown_fields(object, DOCUMENT_FIELDS)?;
    match object.get("version").and_then(Value::as_u64) {
        Some(1) => {}
        Some(version) => return Err(format!("unsupported hooks version {version}")),
        None => return Err("version must be the integer 1".into()),
    }
    object
        .get("hooks")
        .and_then(Value::as_object)
        .ok_or_else(|| "hooks must be an object".to_string())?;
    Ok(())
}

fn parse_hook(
    value: &Value,
    root: &Path,
    plugin_data: &Path,
    plugin_name: &str,
    event: PluginHookEvent,
    index: usize,
) -> Result<PluginHook, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "hook entry must be an object".to_string())?;
    reject_unknown_fields(object, HOOK_FIELDS)?;
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
    let timeout_ms = match object.get("timeout_ms") {
        None => DEFAULT_TIMEOUT_MS,
        Some(value) => value
            .as_u64()
            .filter(|value| (1..=MAX_TIMEOUT_MS).contains(value))
            .ok_or_else(|| format!("timeout_ms must be an integer from 1 to {MAX_TIMEOUT_MS}"))?,
    };
    Ok(PluginHook {
        plugin_name: plugin_name.to_owned(),
        event,
        index,
        launch: StdioLaunch {
            command,
            args,
            env,
            cwd: Some(cwd),
            environment: ProcessEnvironment::Sanitized,
        },
        timeout: Duration::from_millis(timeout_ms),
    })
}

fn parse_event(value: &str) -> Option<PluginHookEvent> {
    Some(match value {
        "on_agent_start" => PluginHookEvent::OnAgentStart,
        "before_model" => PluginHookEvent::BeforeModel,
        "after_model" => PluginHookEvent::AfterModel,
        "before_tool" => PluginHookEvent::BeforeTool,
        "after_tool" => PluginHookEvent::AfterTool,
        "on_error" => PluginHookEvent::OnError,
        "on_agent_end" => PluginHookEvent::OnAgentEnd,
        _ => return None,
    })
}

fn invalid_document(
    warnings: &mut Vec<PluginWarning>,
    message: impl Into<String>,
) -> Vec<PluginHook> {
    warnings.push(PluginWarning::new(
        format!("extensions.{ORCACODE_EXTENSION_NAMESPACE}.hooks"),
        message,
    ));
    Vec::new()
}

fn hook_scope(event: &str, index: Option<usize>) -> String {
    let base = format!("extensions.{ORCACODE_EXTENSION_NAMESPACE}.hooks.{event}");
    index.map_or(base.clone(), |index| format!("{base}[{index}]"))
}
