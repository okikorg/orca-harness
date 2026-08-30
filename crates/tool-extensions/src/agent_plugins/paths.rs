use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use super::PluginError;

pub(super) fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, PluginError> {
    let canonical = fs::canonicalize(path)
        .map_err(|error| PluginError::new(format!("cannot resolve {label}: {error}")))?;
    if !canonical.is_dir() {
        return Err(PluginError::new(format!("{label} must be a directory")));
    }
    Ok(canonical)
}

pub(super) fn normalize_data_boundary(path: &Path) -> Result<PathBuf, PluginError> {
    if !path.is_absolute() {
        return Err(PluginError::new("plugin data path must be absolute"));
    }
    let normalized = normalize_absolute(path)
        .map_err(|message| PluginError::new(format!("invalid plugin data path: {message}")))?;
    if normalized.exists() && !normalized.is_dir() {
        return Err(PluginError::new(
            "plugin data path exists but is not a directory",
        ));
    }
    Ok(normalized)
}

pub(super) fn required_file(root: &Path, name: &str) -> Result<PathBuf, PluginError> {
    match resolve_file(root, name) {
        Ok(Some(path)) => Ok(path),
        Ok(None) => Err(PluginError::new(format!("missing required {name}"))),
        Err(message) => Err(PluginError::new(message)),
    }
}

pub(super) fn optional_file(root: &Path, name: &str) -> Result<Option<PathBuf>, String> {
    resolve_file(root, name)
}

fn resolve_file(root: &Path, name: &str) -> Result<Option<PathBuf>, String> {
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot inspect {name}: {error}")),
        Ok(_) => {}
    }
    let canonical =
        fs::canonicalize(&path).map_err(|error| format!("cannot resolve {name}: {error}"))?;
    if !canonical.starts_with(root) {
        return Err(format!("{name} resolves outside the plugin root"));
    }
    if !canonical.is_file() {
        return Err(format!("{name} must resolve to a regular file"));
    }
    Ok(Some(canonical))
}

pub(super) fn resolve_descendant(boundary: &Path, suffix: &Path) -> Result<PathBuf, String> {
    let candidate = lexical_normalize(&boundary.join(suffix))?;
    if !candidate.starts_with(boundary) {
        return Err("path escapes its allowed boundary".into());
    }
    let resolved = normalize_absolute(&candidate)?;
    if !resolved.starts_with(boundary) {
        return Err("path resolves outside its allowed boundary".into());
    }
    Ok(resolved)
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, String> {
    let lexical = lexical_normalize(path)?;
    let mut existing = lexical.as_path();
    let mut missing = Vec::<OsString>::new();
    loop {
        match fs::canonicalize(existing) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if fs::symlink_metadata(existing).is_ok() {
                    return Err(format!(
                        "cannot resolve existing path {}: {error}",
                        existing.display()
                    ));
                }
                let name = existing.file_name().ok_or_else(|| {
                    format!("cannot resolve existing ancestor of {}", path.display())
                })?;
                missing.push(name.to_os_string());
                existing = existing.parent().ok_or_else(|| {
                    format!("cannot resolve existing ancestor of {}", path.display())
                })?;
            }
            Err(error) => return Err(format!("cannot resolve {}: {error}", existing.display())),
        }
    }
}

fn lexical_normalize(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("path must be absolute".into());
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err("path escapes the filesystem root".into());
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}
