use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::storage::{load_root, mutate_root, mutate_section};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredPlugin {
    pub name: String,
    pub root: PathBuf,
    pub enabled: bool,
}

pub fn stored_plugins() -> Vec<RegisteredPlugin> {
    let Some(root) = load_root() else {
        return Vec::new();
    };
    root["plugins"]
        .as_object()
        .map(|plugins| {
            plugins
                .iter()
                .filter_map(|(name, entry)| {
                    orca_harness_tool_extensions::agent_plugins::validate_agent_plugin_name(name)
                        .ok()?;
                    let fields = entry.as_object()?;
                    let root = fields.get("root")?.as_str()?.trim();
                    let root = PathBuf::from(root);
                    let enabled = fields.get("enabled")?.as_bool()?;
                    root.is_absolute().then(|| RegisteredPlugin {
                        name: name.clone(),
                        root,
                        enabled,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn stored_plugin(name: &str) -> Option<RegisteredPlugin> {
    stored_plugins().into_iter().find(|p| p.name == name)
}

pub fn save_plugin(name: &str, root: impl AsRef<Path>, enabled: bool) -> io::Result<PathBuf> {
    let root = root.as_ref().display().to_string();
    mutate_section("plugins", name, move |entry| {
        *entry = json!({ "root": root, "enabled": enabled });
    })
}

pub fn set_plugin_enabled(name: &str, enabled: bool) -> io::Result<bool> {
    update_plugin(name, None, enabled)
}

pub fn enable_plugin(name: &str, canonical_root: &Path) -> io::Result<bool> {
    update_plugin(name, Some(canonical_root), true)
}

fn update_plugin(name: &str, root: Option<&Path>, enabled: bool) -> io::Result<bool> {
    let found = std::cell::Cell::new(false);
    mutate_root(|config| {
        if let Some(entry) = config
            .get_mut("plugins")
            .and_then(Value::as_object_mut)
            .and_then(|plugins| plugins.get_mut(name))
            .and_then(Value::as_object_mut)
        {
            if let Some(root) = root {
                entry.insert(
                    "root".into(),
                    json!(root.to_str().expect("validated plugin root is UTF-8")),
                );
            }
            entry.insert("enabled".into(), json!(enabled));
            found.set(true);
        }
    })?;
    Ok(found.get())
}

pub fn remove_plugin(name: &str) -> io::Result<bool> {
    let removed = std::cell::Cell::new(false);
    mutate_root(|root| {
        if root
            .get_mut("plugins")
            .and_then(Value::as_object_mut)
            .is_some_and(|plugins| plugins.remove(name).is_some())
        {
            removed.set(true);
        }
    })?;
    Ok(removed.get())
}

pub fn plugin_data_path(name: &str) -> io::Result<PathBuf> {
    let config = super::config_path().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    let path = config
        .parent()
        .expect("config path has a parent")
        .join("plugin-data")
        .join(name);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Create the already-resolved data boundary immediately before a plugin
/// server starts. Static validation and disabled/no-server plugins never call
/// this, so merely loading plugin metadata stays side-effect free.
pub(crate) fn create_plugin_data_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)?;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "plugin data path is not a directory",
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)?;
        if !fs::metadata(path)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "plugin data path is not a directory",
            ));
        }
    }
    Ok(())
}
