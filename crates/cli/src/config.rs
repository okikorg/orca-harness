//! Persistent user configuration. API keys, the active provider, the
//! theme, the last model picked per provider, and per-workspace tool
//! approvals are saved here so a fresh `orcacode` session starts where
//! the last one left off.
//!
//! The file is JSON at `$ORCA_CONFIG_DIR/config.json`, defaulting to
//! `~/.config/orcacode/config.json` (`%APPDATA%\orcacode` on Windows),
//! and is created with owner-only permissions. Shape:
//!
//! ```json
//! {
//!   "api_keys": { "openrouter": "sk-or-...", "openai": "sk-..." },
//!   "models": { "openrouter": "openrouter/auto", "local": "qwen3.5:9b" },
//!   "provider": "openrouter",
//!   "theme": "nord",
//!   "view": "split",
//!   "approvals": { "/abs/workspace/root": ["shell", "write_file"] },
//!   "extensions": { "truncation": true, "retry": false },
//!   "mcp": {
//!     "docs": { "command": "npx -y mcp-remote https://…", "enabled": true },
//!     "fetch": "uvx mcp-server-fetch"
//!   }
//! }
//! ```
//!
//! An MCP entry may be a bare command string (always enabled) or the
//! object form above; toggling one in `/mcp` promotes it to the object.

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

/// The saved on/off override for a user-toggleable extension. `None`
/// means nothing was saved and the extension's default applies.
pub fn stored_extension(name: &str) -> Option<bool> {
    load_root()?["extensions"][name].as_bool()
}

/// Save an extension override; agent rebuilds honor it from then on.
pub fn save_extension(name: &str, enabled: bool) -> io::Result<PathBuf> {
    mutate_section("extensions", name, move |entry| *entry = json!(enabled))
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

/// Save (or replace) an MCP server's launch command, keeping whatever
/// enabled state it already had; the worker reconnects so it applies to
/// the next run.
pub fn save_mcp_server(name: &str, command: &str) -> io::Result<PathBuf> {
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

// In-memory stand-in for the config file under test: tests must never
// touch the developer's real config, and each test thread gets its own
// isolated copy so tests cannot interfere with each other.
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

fn load_root() -> Option<Value> {
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
fn mutate_root(edit: impl FnOnce(&mut serde_json::Map<String, Value>)) -> io::Result<PathBuf> {
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
fn mutate_section(
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
mod tests {
    use super::*;

    fn raw() -> String {
        TEST_FILE
            .with(|file| file.borrow().clone())
            .expect("config written")
    }

    fn seed(body: &str) {
        TEST_FILE.with(|file| *file.borrow_mut() = Some(body.to_string()));
    }

    #[test]
    fn save_then_load_round_trips_per_provider() {
        assert_eq!(stored_key("openrouter"), None);
        save_key("openrouter", "sk-or-123").unwrap();
        save_key("openai", "sk-456").unwrap();

        assert_eq!(stored_key("openrouter").as_deref(), Some("sk-or-123"));
        assert_eq!(stored_key("openai").as_deref(), Some("sk-456"));
        // Overwriting replaces, and never clobbers the other provider.
        save_key("openrouter", "sk-or-789").unwrap();
        assert_eq!(stored_key("openrouter").as_deref(), Some("sk-or-789"));
        assert_eq!(stored_key("openai").as_deref(), Some("sk-456"));
    }

    #[test]
    fn provider_theme_and_models_round_trip_alongside_keys() {
        assert_eq!(stored_provider(), None);
        assert_eq!(stored_theme(), None);
        assert_eq!(stored_model("openrouter"), None);

        save_key("openrouter", "sk-or-1").unwrap();
        save_provider("openrouter").unwrap();
        save_theme("nord").unwrap();
        save_model("openrouter", "anthropic/claude-sonnet-4").unwrap();
        save_model("local", "qwen3.5:9b").unwrap();

        assert_eq!(stored_provider().as_deref(), Some("openrouter"));
        assert_eq!(stored_theme().as_deref(), Some("nord"));
        assert_eq!(
            stored_model("openrouter").as_deref(),
            Some("anthropic/claude-sonnet-4")
        );
        assert_eq!(stored_model("local").as_deref(), Some("qwen3.5:9b"));
        // Preferences never disturb the keys section.
        assert_eq!(stored_key("openrouter").as_deref(), Some("sk-or-1"));
    }

    #[test]
    fn save_preserves_unknown_fields() {
        seed(r#"{"future_field": true, "api_keys": {"openai": "sk-old"}}"#);
        save_key("openrouter", "sk-or-new").unwrap();
        save_theme("mono").unwrap();

        let root: Value = serde_json::from_str(&raw()).unwrap();
        assert_eq!(root["future_field"], true);
        assert_eq!(root["api_keys"]["openai"], "sk-old");
        assert_eq!(root["api_keys"]["openrouter"], "sk-or-new");
        assert_eq!(root["theme"], "mono");
    }

    #[test]
    fn approvals_are_scoped_per_workspace_and_removable() {
        assert!(stored_approvals("/repo/a").is_empty());

        save_approval("/repo/a", "shell").unwrap();
        save_approval("/repo/a", "write_file").unwrap();
        save_approval("/repo/a", "shell").unwrap(); // idempotent
        save_approval("/repo/b", "pykernel").unwrap();

        assert_eq!(stored_approvals("/repo/a"), ["shell", "write_file"]);
        // Trust does not leak across workspaces.
        assert_eq!(stored_approvals("/repo/b"), ["pykernel"]);

        remove_approval("/repo/a", "shell").unwrap();
        assert_eq!(stored_approvals("/repo/a"), ["write_file"]);
        assert_eq!(stored_approvals("/repo/b"), ["pykernel"]);
        // Approvals coexist with the other sections.
        save_key("openai", "sk-1").unwrap();
        assert_eq!(stored_approvals("/repo/a"), ["write_file"]);
    }

    #[test]
    fn extension_overrides_round_trip_and_coexist() {
        assert_eq!(stored_extension("retry"), None);
        save_extension("retry", true).unwrap();
        save_extension("truncation", false).unwrap();
        assert_eq!(stored_extension("retry"), Some(true));
        assert_eq!(stored_extension("truncation"), Some(false));
        // Overwriting flips just the one entry.
        save_extension("retry", false).unwrap();
        assert_eq!(stored_extension("retry"), Some(false));
        assert_eq!(stored_extension("truncation"), Some(false));
        // Non-boolean garbage reads as unset, not as a state.
        seed(r#"{"extensions": {"retry": "yes"}}"#);
        assert_eq!(stored_extension("retry"), None);
    }

    /// A (name, command, enabled) view of the stored servers.
    #[cfg(test)]
    fn listed() -> Vec<(String, String, bool)> {
        stored_mcp_servers()
            .into_iter()
            .map(|s| (s.name, s.command, s.enabled))
            .collect()
    }

    #[test]
    fn mcp_servers_round_trip_and_are_removable() {
        assert!(stored_mcp_servers().is_empty());

        save_mcp_server("docs", "npx -y some-server /tmp").unwrap();
        save_mcp_server("db", "uvx db-server").unwrap();
        assert_eq!(
            listed(),
            [
                ("db".to_string(), "uvx db-server".to_string(), true),
                (
                    "docs".to_string(),
                    "npx -y some-server /tmp".to_string(),
                    true
                ),
            ]
        );

        // Re-adding a name replaces its command.
        save_mcp_server("docs", "npx -y other-server").unwrap();
        assert_eq!(listed()[1].1, "npx -y other-server");

        remove_mcp_server("docs").unwrap();
        assert_eq!(stored_mcp_servers().len(), 1);
        // Removing an unknown name is a no-op, not an error.
        remove_mcp_server("nope").unwrap();
        assert_eq!(stored_mcp_servers().len(), 1);
        // Servers coexist with the other sections.
        save_key("openai", "sk-1").unwrap();
        assert_eq!(stored_mcp_servers().len(), 1);

        // Blank or unusable commands read as absent, not as servers:
        // a blank string, a number, an object with no command, and an
        // object whose command is blank.
        seed(
            r#"{"mcp": {
                "a": "  ",
                "b": 7,
                "c": "run c",
                "d": {"enabled": true},
                "e": {"command": " "},
                "f": {"command": "run f", "enabled": false}
            }}"#,
        );
        assert_eq!(
            listed(),
            [
                ("c".to_string(), "run c".to_string(), true),
                ("f".to_string(), "run f".to_string(), false),
            ]
        );
    }

    #[test]
    fn toggling_preserves_the_command_and_promotes_the_string_shape() {
        // The bare-string shape written before toggles existed reads as
        // enabled and survives a disable.
        seed(r#"{"mcp": {"docs": "npx -y some-server"}}"#);
        assert_eq!(
            listed(),
            [("docs".into(), "npx -y some-server".into(), true)]
        );

        set_mcp_enabled("docs", false).unwrap();
        assert_eq!(
            listed(),
            [("docs".to_string(), "npx -y some-server".to_string(), false)]
        );

        // Re-saving a disabled server's command keeps it disabled: an
        // edit is not an enable.
        save_mcp_server("docs", "npx -y other-server").unwrap();
        assert_eq!(
            listed(),
            [("docs".to_string(), "npx -y other-server".to_string(), false)]
        );

        set_mcp_enabled("docs", true).unwrap();
        assert!(listed()[0].2);

        // Toggling a name that is not configured is a no-op, not an
        // error and not a new entry.
        set_mcp_enabled("nope", false).unwrap();
        assert_eq!(stored_mcp_servers().len(), 1);
        seed("{}");
        set_mcp_enabled("nope", false).unwrap();
        assert!(stored_mcp_servers().is_empty());
    }

    #[test]
    fn corrupt_config_is_replaced_not_fatal() {
        seed("not json {");
        assert_eq!(stored_key("openai"), None);
        save_key("openai", "sk-new").unwrap();
        assert_eq!(stored_key("openai").as_deref(), Some("sk-new"));
    }

    /// The real on-disk writer must produce owner-only files, and must
    /// tighten a pre-existing looser file when rewriting it.
    #[cfg(unix)]
    #[test]
    fn config_file_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("orcacode-perm-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        write_private(&path, "{}\n").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config must not be group/world readable");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, "{}\n").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "rewrites tighten a loose pre-existing file");

        let _ = fs::remove_dir_all(&dir);
    }
}
