//! Agent Skills discovery at the fixed Agent Plugins `skills/` location.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::skills::{Discovered, Shadowed, Skill, SkillFailure};

use super::PluginWarning;

pub(super) fn load(
    plugin_root: &Path,
    plugin_name: &str,
    warnings: &mut Vec<PluginWarning>,
) -> Discovered {
    let component = plugin_root.join("skills");
    match fs::symlink_metadata(&component) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Discovered::default(),
        Err(error) => {
            warnings.push(PluginWarning::new(
                "skills",
                format!("cannot inspect skills component: {error}"),
            ));
            return Discovered::default();
        }
    }
    let component = match contained_canonical(plugin_root, &component) {
        Ok(path) => path,
        Err(reason) => {
            warnings.push(PluginWarning::new("skills", reason));
            return Discovered::default();
        }
    };
    if !fs::metadata(&component).is_ok_and(|metadata| metadata.is_dir()) {
        warnings.push(PluginWarning::new(
            "skills",
            "component must resolve to a directory",
        ));
        return Discovered::default();
    }

    let entries = match fs::read_dir(&component) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(PluginWarning::new(
                "skills",
                format!("cannot read skills component: {error}"),
            ));
            return Discovered::default();
        }
    };
    let mut children = entries.filter_map(Result::ok).collect::<Vec<_>>();
    children.sort_by_key(|entry| entry.file_name());
    let label = format!("plugin:{plugin_name}");
    let mut found = Discovered::default();

    for entry in children {
        let display_name = entry.file_name().to_string_lossy().into_owned();
        let candidate = entry.path();
        if !fs::metadata(&candidate).is_ok_and(|metadata| metadata.is_dir()) {
            continue;
        }
        let skill_file = candidate.join("SKILL.md");
        if !fs::metadata(&skill_file).is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        match load_one(plugin_root, &candidate, &skill_file, &display_name, &label) {
            Ok(skill) => match found.skills.iter().find(|prior| prior.name == skill.name) {
                Some(prior) => found.shadowed.push(Shadowed {
                    name: skill.name,
                    root: label.clone(),
                    by: prior.root.clone(),
                    dir: skill.dir,
                }),
                None => found.skills.push(skill),
            },
            Err(reason) => {
                warnings.push(PluginWarning::new(
                    format!("skills.{display_name}"),
                    reason.clone(),
                ));
                found.failures.push(SkillFailure {
                    name: display_name,
                    root: label.clone(),
                    reason,
                    dir: candidate,
                });
            }
        }
    }
    found
}

fn load_one(
    plugin_root: &Path,
    candidate: &Path,
    skill_file: &Path,
    directory_name: &str,
    label: &str,
) -> Result<Skill, String> {
    let dir = contained_canonical(plugin_root, candidate)?;
    let file = contained_canonical(plugin_root, skill_file)?;
    let bytes = fs::metadata(&file)
        .map_err(|error| format!("cannot inspect SKILL.md: {error}"))?
        .len();
    let text = fs::read_to_string(&file)
        .map_err(|error| format!("cannot read SKILL.md as UTF-8: {error}"))?;
    let front = parse_standard_frontmatter(&text)?;
    let name = front.name;
    validate_name(&name, directory_name)?;
    let description = front.description;
    if description.is_empty() {
        return Err("no `description` in the frontmatter".into());
    }
    if description.chars().count() > 1024 {
        return Err("description exceeds 1024 characters".into());
    }
    if let Some(compatibility) = front.compatibility {
        let length = compatibility.chars().count();
        if !(1..=500).contains(&length) {
            return Err("compatibility must contain 1-500 characters".into());
        }
    }
    Ok(Skill {
        name,
        description,
        dir,
        file,
        root: label.to_string(),
        bytes,
    })
}

struct StandardFrontmatter {
    name: String,
    description: String,
    compatibility: Option<String>,
}

fn parse_standard_frontmatter(text: &str) -> Result<StandardFrontmatter, String> {
    let text = text.trim_start_matches('\u{feff}');
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or("no `---` frontmatter at the top of the file")?;
    let mut end = None;
    let mut consumed = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']).trim_end() == "---" {
            end = Some(consumed);
            break;
        }
        consumed += line.len();
    }
    let yaml = &rest[..end.ok_or("frontmatter is never closed by a `---` line")?];
    let value: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|error| format!("invalid YAML frontmatter: {error}"))?;
    let map = value
        .as_mapping()
        .ok_or("frontmatter must be a YAML mapping")?;
    let required_string = |key: &str| {
        map.get(serde_yaml::Value::String(key.into()))
            .and_then(serde_yaml::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("`{key}` must be a string"))
    };
    let optional_string = |key: &str| -> Result<Option<String>, String> {
        let Some(value) = map.get(serde_yaml::Value::String(key.into())) else {
            return Ok(None);
        };
        value
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| format!("`{key}` must be a string"))
    };
    optional_string("license")?;
    optional_string("allowed-tools")?;
    if let Some(metadata) = map.get(serde_yaml::Value::String("metadata".into())) {
        let metadata = metadata
            .as_mapping()
            .ok_or("`metadata` must be a string-to-string mapping")?;
        if metadata
            .iter()
            .any(|(key, value)| key.as_str().is_none() || value.as_str().is_none())
        {
            return Err("`metadata` must be a string-to-string mapping".into());
        }
    }
    Ok(StandardFrontmatter {
        name: required_string("name")?,
        description: required_string("description")?,
        compatibility: optional_string("compatibility")?,
    })
}

fn validate_name(name: &str, directory_name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name == directory_name
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && !name.contains("--");
    valid.then_some(()).ok_or_else(|| {
        format!(
            "invalid Agent Skill name {name:?}; use 1-64 lowercase letters, digits, or single hyphens and match directory {directory_name:?}"
        )
    })
}

fn contained_canonical(plugin_root: &Path, path: &Path) -> Result<PathBuf, String> {
    let resolved = fs::canonicalize(path)
        .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;
    if resolved.starts_with(plugin_root) {
        Ok(resolved)
    } else {
        Err(format!(
            "resolved path escapes plugin root: {}",
            path.display()
        ))
    }
}
