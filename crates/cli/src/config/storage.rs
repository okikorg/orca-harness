// Persistent user configuration. API keys, the active provider, the
// theme, the last model picked per provider, and per-workspace tool
// approvals are saved here so a fresh `orcacode` session starts where
// the last one left off.
//
// The file is JSON at `$ORCA_CONFIG_DIR/config.json`, defaulting to
// `~/.config/orcacode/config.json` (`%APPDATA%\orcacode` on Windows),
// and is created with owner-only permissions. Shape:
//
// ```json
// {
//   "api_keys": { "openrouter": "sk-or-...", "openai": "sk-..." },
//   "models": { "openrouter": "openrouter/auto", "local": "qwen3.5:9b" },
//   "provider": "openrouter",
//   "theme": "nord",
//   "view": "split",
//   "approvals": { "/abs/workspace/root": ["shell", "write_file"] },
//   "extensions": { "truncation": true, "retry": false },
//   "mcp": {
//     "docs": { "command": "npx -y mcp-remote https://…", "enabled": true },
//     "fetch": "uvx mcp-server-fetch"
//   },
//   "skills": { "release": false }
// }
// ```
//
// An MCP entry may be a bare command string (always enabled) or the
// object form above; toggling one in `/mcp` promotes it to the object.
//
// Skills are discovered on disk rather than declared, so the `skills`
// section holds overrides only: a name appears there only once the user
// has turned it off in `/skills`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// The config file path, or `None` when no home directory is resolvable.
pub fn config_path() -> Option<PathBuf> {
    let explicit = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let dir = if let Some(dir) = explicit("ORCA_CONFIG_DIR") {
        PathBuf::from(dir)
    } else if let Some(xdg) = explicit("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("orcacode")
    } else if cfg!(windows) {
        PathBuf::from(explicit("APPDATA")?).join("orcacode")
    } else {
        PathBuf::from(explicit("HOME")?)
            .join(".config")
            .join("orcacode")
    };
    Some(dir.join("config.json"))
}

/// The sessions directory, or `None` when no home directory is
/// resolvable. Sessions live next to the config file.
pub fn sessions_dir() -> Option<PathBuf> {
    Some(config_path()?.parent()?.join("sessions"))
}

/// The single local SQLite database used for deliberate durable memory.
pub fn memory_path() -> Option<PathBuf> {
    Some(config_path()?.parent()?.join("memory.sqlite3"))
}

/// The saved API key for a provider label, ignoring blank values.
pub fn stored_key(provider: &str) -> Option<String> {
    stored_str(Some("api_keys"), provider)
}

/// Save an API key under the provider label. Returns the path written
/// so the UI can tell the user where the key lives.
pub fn save_key(provider: &str, key: &str) -> io::Result<PathBuf> {
    save_str(Some("api_keys"), provider, key)
}

/// The model last picked while this provider was active.
pub fn stored_model(provider: &str) -> Option<String> {
    stored_str(Some("models"), provider)
}

pub fn save_model(provider: &str, model: &str) -> io::Result<PathBuf> {
    save_str(Some("models"), provider, model)
}

/// The provider active when the last session ended.
pub fn stored_provider() -> Option<String> {
    stored_str(None, "provider")
}

pub fn save_provider(label: &str) -> io::Result<PathBuf> {
    save_str(None, "provider", label)
}

/// The theme slug last picked, as accepted by `ThemeName::from_str`.
pub fn stored_theme() -> Option<String> {
    stored_str(None, "theme")
}

pub fn save_theme(slug: &str) -> io::Result<PathBuf> {
    save_str(None, "theme", slug)
}

/// The transcript layout last selected in `/settings`.
pub fn stored_view() -> Option<String> {
    stored_str(None, "view")
}

pub fn save_view(slug: &str) -> io::Result<PathBuf> {
    save_str(None, "view", slug)
}

pub fn stored_inspector() -> Option<String> {
    stored_str(None, "inspector")
}

pub fn save_inspector(slug: &str) -> io::Result<PathBuf> {
    save_str(None, "inspector", slug)
}

pub fn stored_transcript_spacing() -> Option<String> {
    stored_str(None, "transcript_spacing")
}

pub fn save_transcript_spacing(slug: &str) -> io::Result<PathBuf> {
    save_str(None, "transcript_spacing", slug)
}

/// The mark vocabulary last selected in `/settings` (`minimal` or `glyph`).
pub fn stored_style() -> Option<String> {
    stored_str(None, "style")
}

pub fn save_style(slug: &str) -> io::Result<PathBuf> {
    save_str(None, "style", slug)
}

/// Persist the complete live subagent preference set. Keeping this as one
/// object prevents a partially-updated picker choice from leaving related
/// routing fields out of sync.
pub fn save_subagent_settings(settings: &orca_harness_tools::SubagentDepth) -> io::Result<PathBuf> {
    let preferred = ["local", "flash", "mid", "frontier"]
        .into_iter()
        .filter_map(|tier| {
            settings
                .preferred_model(tier)
                .map(|model| (tier.to_string(), json!(model)))
        })
        .collect::<serde_json::Map<String, Value>>();
    let mut value = json!({ "route": settings.model_route(), "preferred": preferred });
    for field in crate::subagent_settings::NUMERIC {
        value[field.key] = json!((field.read)(settings));
    }
    mutate_root(move |root| {
        root.insert("subagents".into(), value);
    })
}

/// Apply saved subagent preferences to a handle whose model catalog has
/// already been attached. Invalid or stale routes are ignored by the handle,
/// while independent governance values still load.
pub fn load_subagent_settings(settings: &orca_harness_tools::SubagentDepth) {
    let Some(saved) = load_root().and_then(|root| root.get("subagents").cloned()) else {
        return;
    };
    for field in crate::subagent_settings::NUMERIC {
        if let Some(value) = saved[field.key]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
        {
            if field.parse(&value.to_string()).is_ok() {
                (field.write)(settings, value);
            }
        }
    }
    if let Some(preferred) = saved["preferred"].as_object() {
        for (tier, model) in preferred {
            if let Some(model) = model.as_str() {
                settings.set_preferred_model(tier, model.to_string());
            }
        }
    }
    let route = saved["route"].as_str().map(str::to_string);
    settings.set_model_route(route);
}

/// The saved on/off override for a user-toggleable extension. `None`
/// means nothing was saved and the extension's default applies.
pub fn stored_extension(name: &str) -> Option<bool> {
    load_root()?["extensions"][name].as_bool()
}

/// Save an extension override; agent rebuilds honor it from then on.
pub fn save_extension(name: &str, enabled: bool) -> io::Result<PathBuf> {
    mutate_section("extensions", name, move |entry| *entry = json!(enabled))
}

/// The saved on/off override for a discovered skill. `None` means the
/// user never touched it, which reads as on — a skill dropped into one
/// of the skill directories works with no second step.
pub fn stored_skill_enabled(name: &str) -> Option<bool> {
    load_root()?["skills"][name].as_bool()
}

/// Save a skill override; the next rescan and agent rebuild honor it.
pub fn save_skill_enabled(name: &str, enabled: bool) -> io::Result<PathBuf> {
    mutate_section("skills", name, move |entry| *entry = json!(enabled))
}

/// Forget a skill's override entirely — used when the skill itself is
/// deleted, so reinstalling the same name later does not come back
/// silently switched off.
pub fn forget_skill(name: &str) -> io::Result<PathBuf> {
    let name = name.to_string();
    mutate_root(move |root| {
        if let Some(skills) = root.get_mut("skills").and_then(Value::as_object_mut) {
            skills.remove(&name);
        }
    })
}

/// Tools the user chose to always allow in this workspace (keyed by the
/// canonicalized workspace root). Read at approval time so revocations
/// apply immediately; any read failure yields an empty list, which
/// fails closed — the tool just prompts again.
pub fn stored_approvals(workspace: &str) -> Vec<String> {
    let Some(root) = load_root() else {
        return Vec::new();
    };
    root["approvals"][workspace]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Remember `tool` as always-allowed in this workspace.
pub fn save_approval(workspace: &str, tool: &str) -> io::Result<PathBuf> {
    let tool = tool.to_string();
    mutate_section("approvals", workspace, move |entry| {
        if !entry.is_array() {
            *entry = json!([]);
        }
        let tools = entry.as_array_mut().expect("approvals entry is an array");
        if !tools.iter().any(|t| t.as_str() == Some(&tool)) {
            tools.push(json!(tool));
        }
    })
}

/// Forget a saved always-allow; the tool prompts again from now on.
pub fn remove_approval(workspace: &str, tool: &str) -> io::Result<PathBuf> {
    let tool = tool.to_string();
    mutate_section("approvals", workspace, move |entry| {
        if let Some(tools) = entry.as_array_mut() {
            tools.retain(|t| t.as_str() != Some(&tool));
        }
    })
}

/// One configured MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    /// Disabled servers stay configured but are not connected, so the
    /// /mcp overlay can toggle one off without losing its command.
    pub enabled: bool,
}

/// Configured MCP servers, blank commands ignored. The config map is
/// ordered, so listings and reconnect order are deterministic.
///
/// Both entry shapes are accepted: a bare command string (always
/// enabled — the shape written before toggles existed) and the object
/// form `{"command": "...", "enabled": false}`.
pub fn stored_mcp_servers() -> Vec<McpServer> {
    let Some(root) = load_root() else {
        return Vec::new();
    };
    root["mcp"]
        .as_object()
        .map(|servers| {
            servers
                .iter()
                .filter_map(|(name, entry)| {
                    let command = match entry {
                        Value::Object(fields) => fields.get("command")?.as_str()?,
                        other => other.as_str()?,
                    }
                    .trim();
                    (!command.is_empty()).then(|| McpServer {
                        name: name.clone(),
                        command: command.to_string(),
                        // Anything but an explicit `false` is enabled,
                        // so a hand-edited config never silently hides
                        // a server.
                        enabled: entry["enabled"] != json!(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Validate a named stdio MCP launch command without reading or writing config.
/// Only the executable token is checked; flags and URLs may be arguments.
pub fn validate_mcp_server(name: &str, command: &str) -> io::Result<()> {
    let guidance = "usage: /mcp add <name> <command> — MCP servers use stdio; provide an executable followed by its arguments, e.g. /mcp add docs npx -y mcp-remote https://example.com/mcp (not --transport or --url before the executable)";
    // Names become part of model-facing tool names (mcp__<name>__<tool>).
    if name.is_empty()
        || name.starts_with('-')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid server name: {name} — use a non-empty name with ASCII letters, digits, - and _ only, not starting with '-'; {guidance}"),
        ));
    }
    let executable = command.split_whitespace().next().unwrap_or("");
    let lower = executable.to_ascii_lowercase();
    if executable.is_empty()
        || executable.starts_with('-')
        || lower.starts_with("http://")
        || lower.starts_with("https://")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid MCP executable — must be non-empty, not start with '-', and not be an HTTP(S) URL; {guidance}"),
        ));
    }
    Ok(())
}

/// Save (or replace) an MCP server's launch command, keeping whatever
/// enabled state it already had; the worker reconnects so it applies to
/// the next run.
pub fn save_mcp_server(name: &str, command: &str) -> io::Result<PathBuf> {
    validate_mcp_server(name, command)?;
    let command = command.to_string();
    mutate_section("mcp", name, move |entry| {
        let enabled = entry["enabled"] != json!(false);
        *entry = json!({ "command": command, "enabled": enabled });
    })
}

/// Enable or disable a configured server without forgetting its
/// command. Unknown names are a no-op — the caller lists first.
pub fn set_mcp_enabled(name: &str, enabled: bool) -> io::Result<PathBuf> {
    let name = name.to_string();
    mutate_root(move |root| {
        let Some(entry) = root
            .get_mut("mcp")
            .and_then(Value::as_object_mut)
            .and_then(|servers| servers.get_mut(&name))
        else {
            return;
        };
        // Promote the bare-string shape on first toggle.
        if let Some(command) = entry.as_str() {
            *entry = json!({ "command": command });
        }
        if let Some(fields) = entry.as_object_mut() {
            fields.insert("enabled".into(), json!(enabled));
        }
    })
}

/// Forget a configured MCP server; the next reconnect drops its tools.
pub fn remove_mcp_server(name: &str) -> io::Result<PathBuf> {
    let name = name.to_string();
    mutate_root(move |root| {
        if let Some(servers) = root.get_mut("mcp").and_then(Value::as_object_mut) {
            servers.remove(&name);
        }
    })
}

// Thread-local storage keeps tests isolated from each other and the real config.
#[cfg(test)]
thread_local! {
    static TEST_FILE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

fn read_config() -> Option<String> {
    #[cfg(test)]
    {
        TEST_FILE.with(|file| file.borrow().clone())
    }
    #[cfg(not(test))]
    {
        fs::read_to_string(config_path()?).ok()
    }
}

fn write_config(path: &Path, body: &str) -> io::Result<()> {
    #[cfg(test)]
    {
        let _ = path;
        TEST_FILE.with(|file| *file.borrow_mut() = Some(body.to_string()));
        Ok(())
    }
    #[cfg(not(test))]
    {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
            restrict(dir, 0o700)?;
        }
        write_private(path, body)
    }
}

pub(super) fn load_root() -> Option<Value> {
    serde_json::from_str(&read_config()?)
        .ok()
        .filter(Value::is_object)
}

/// A non-blank string at `root[field]`, or `root[section][field]`.
fn stored_str(section: Option<&str>, field: &str) -> Option<String> {
    let root = load_root()?;
    let value = match section {
        Some(section) => &root[section][field],
        None => &root[field],
    };
    value
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from)
}

/// Set one string field, creating the file if needed. Other fields in
/// an existing file are preserved.
fn save_str(section: Option<&str>, field: &str, value: &str) -> io::Result<PathBuf> {
    let value = json!(value);
    match section {
        Some(section) => mutate_section(section, field, move |entry| *entry = value),
        None => mutate_root(move |root| {
            root.insert(field.into(), value);
        }),
    }
}

/// Load, edit, and rewrite the config, preserving unrelated fields.
pub(super) fn mutate_root(
    edit: impl FnOnce(&mut serde_json::Map<String, Value>),
) -> io::Result<PathBuf> {
    let path = config_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory for config"))?;
    let mut root = load_root().unwrap_or_else(|| json!({}));
    edit(root.as_object_mut().expect("root is an object"));
    let mut body = serde_json::to_string_pretty(&root)?;
    body.push('\n');
    write_config(&path, &body)?;
    Ok(path)
}

/// Edit one value inside a named object section (`api_keys`, `models`,
/// `approvals`), creating the section if needed.
pub(super) fn mutate_section(
    section: &str,
    field: &str,
    edit: impl FnOnce(&mut Value),
) -> io::Result<PathBuf> {
    let field = field.to_string();
    let section = section.to_string();
    mutate_root(move |root| {
        let entry = root.entry(section).or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
        edit(
            entry
                .as_object_mut()
                .expect("section is an object")
                .entry(field)
                .or_insert(Value::Null),
        );
    })
}

/// Write the file readable by the owner only; the mode is applied at
/// creation so the key is never on disk with looser permissions, and
/// re-applied after to tighten a pre-existing file.
#[cfg(unix)]
fn write_private(path: &Path, body: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(body.as_bytes())?;
    restrict(path, 0o600)
}

#[cfg(not(unix))]
fn write_private(path: &Path, body: &str) -> io::Result<()> {
    fs::write(path, body)
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(all(not(unix), not(test)))]
fn restrict(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
include!("tests.rs");
