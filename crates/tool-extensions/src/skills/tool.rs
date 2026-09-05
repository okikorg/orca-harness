//! `skill` — load one skill's instructions, or a text file beside them.
//!
//! One tool, not one per skill: N near-identical tools would bloat the
//! list the model re-reads every turn. The catalog rides in this tool's
//! description and its `name` enum instead, so a `/skills` toggle takes
//! effect through the ordinary agent rebuild.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tokio::fs;

use orca_harness_core::{Tool, ToolContext, ToolError, ToolSchema};

use crate::skills::skill::Skill;

/// Characters returned per call. Matches `read_tool_result`'s default
/// slice, and keeps a long skill from filling the context in one go.
const CHUNK: usize = 8 * 1024;

/// Cap on the `resources` listing, so a skill directory that happens to
/// hold a vendored tree does not become the response.
const MAX_RESOURCES: usize = 50;

/// Characters of each description carried in the catalog. The catalog
/// sits in the tool schema, which the model re-reads every turn, and the
/// compatibility roots mean a user's whole `~/.claude/skills` collection
/// can show up at once — a paragraph-long description per entry adds up.
/// The full text is one `skill` call away.
const CATALOG_DESCRIPTION: usize = 160;

pub struct SkillTool {
    skills: Vec<Skill>,
    chunk: usize,
}

impl SkillTool {
    /// Build the tool over the skills the host decided to expose
    /// (discovered, minus whatever the user turned off).
    pub fn new(skills: Vec<Skill>) -> Self {
        Self {
            skills,
            chunk: CHUNK,
        }
    }

    /// Floored at one character's worth of bytes: a smaller chunk could
    /// fail to advance past a multi-byte character, and a caller looping
    /// on `nextOffset` would never terminate.
    pub fn chunk(mut self, chars: usize) -> Self {
        self.chunk = chars.max(4);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    fn find(&self, name: &str) -> Result<&Skill, ToolError> {
        self.skills
            .iter()
            .find(|skill| skill.name == name)
            .ok_or_else(|| {
                let known = self
                    .skills
                    .iter()
                    .map(|skill| skill.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                ToolError::msg(match known.is_empty() {
                    true => format!("unknown skill: {name} — none are loaded"),
                    false => format!("unknown skill: {name} — available: {known}"),
                })
            })
    }

    fn description(&self) -> String {
        let mut text = String::from(
            "Load a skill: instructions the user wrote for a specific task, plus the \
             text files they point at. Call this before starting work a listed skill \
             covers, then follow what it says. Pass `resource` to read a file the \
             instructions refer to, and `offset` to continue a long one.",
        );
        if !self.skills.is_empty() {
            text.push_str("\nAvailable skills:");
            for skill in &self.skills {
                text.push_str(&format!(
                    "\n  {} — {}",
                    skill.name,
                    clip(&skill.description, CATALOG_DESCRIPTION)
                ));
            }
        }
        text
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn schema(&self) -> ToolSchema {
        let names: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
        ToolSchema {
            name: "skill".into(),
            description: self.description(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "enum": names,
                        "description": "Name of the skill to load, from the list above.",
                    },
                    "resource": {
                        "type": "string",
                        "description": "Optional path, relative to the skill's own folder, of a text file to read instead of its instructions. Defaults to SKILL.md.",
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Resume at this byte offset; use the nextOffset from the previous call. Defaults to 0.",
                    },
                },
                "required": ["name"]
            }),
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let name = input
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`name` (string) is required"))?;
        let skill = self.find(name)?;
        let resource = input.get("resource").and_then(Value::as_str);
        let offset = match input.get("offset") {
            None | Some(Value::Null) => 0,
            Some(value) => value
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| ToolError::msg("`offset` must be a non-negative integer"))?,
        };

        let path = match resource {
            Some(rel) => contain(&skill.dir, rel)?,
            None => skill.file.clone(),
        };
        let bytes = fs::read(&path)
            .await
            .map_err(|e| ToolError::msg(format!("cannot read {}: {e}", path.display())))?;
        let text = String::from_utf8(bytes).map_err(|_| {
            ToolError::msg(format!(
                "{} is not UTF-8 text; skills carry text only",
                path.display()
            ))
        })?;
        // The frontmatter is the catalog's business, not the model's: it
        // has already seen the name and description.
        let text = match resource {
            Some(_) => text.as_str(),
            None => body_of(&text),
        };
        let (slice, next) = page(text, offset, self.chunk);

        let mut out = Map::new();
        out.insert("name".into(), json!(skill.name));
        out.insert("root".into(), json!(skill.root));
        out.insert("path".into(), json!(path.display().to_string()));
        if let Some(rel) = resource {
            out.insert("resource".into(), json!(rel));
        }
        out.insert("instructions".into(), json!(slice));
        if let Some(next) = next {
            out.insert("nextOffset".into(), json!(next));
        }
        // Only worth listing on the way in: once the model is paging, it
        // knows what is there.
        if resource.is_none() && offset == 0 {
            let resources = resources_of(&skill.dir);
            if !resources.is_empty() {
                out.insert("resources".into(), json!(resources));
            }
        }
        Ok(Value::Object(out))
    }
}

/// Everything after the frontmatter fence, or the whole file when there
/// is none (discovery would have rejected that file, but the tool reads
/// from disk fresh and the file may have been edited since).
fn body_of(text: &str) -> &str {
    let text = text.trim_start_matches('\u{feff}');
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return text;
    };
    let mut consumed = 0;
    for line in rest.split_inclusive('\n') {
        consumed += line.len();
        if line.trim_end() == "---" {
            return rest[consumed..].trim_start_matches('\n');
        }
    }
    text
}

/// One chunk starting at `offset`, plus where to resume. Offsets are
/// byte offsets snapped down to character boundaries, so a model that
/// echoes back `nextOffset` always lands somewhere valid.
fn page(text: &str, offset: usize, chunk: usize) -> (&str, Option<usize>) {
    if offset >= text.len() {
        return ("", None);
    }
    let start = floor_boundary(text, offset);
    let end = floor_boundary(text, start.saturating_add(chunk).min(text.len()));
    let end = end.max(start);
    (&text[start..end], (end < text.len()).then_some(end))
}

/// `text` shortened to `max` characters, ending in `…` when cut.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((cut, _)) => format!("{}…", text[..cut].trim_end()),
    }
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Files beside `SKILL.md`, relative to the skill directory, so the
/// model can ask for one by name without a `list_dir` round trip.
///
/// Bounded twice over — `MAX_RESOURCES` entries and `MAX_DEPTH` levels —
/// because a skill folder can contain anything, including a vendored
/// tree, and this walk runs inline on the call.
///
/// Only a symlink can point out of the folder, so only symlinks are
/// resolved and checked against the folder's real path; a plain file or
/// directory is inside by construction. That keeps the common case to
/// one `read_dir` — the entry type comes with the entry — instead of a
/// `realpath` per entry, which walks every component of the path.
fn resources_of(dir: &Path) -> Vec<String> {
    const MAX_DEPTH: usize = 3;
    let mut base: Option<PathBuf> = None;
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((current, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        let mut found: Vec<(PathBuf, std::fs::FileType)> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_type().ok().map(|kind| (e.path(), kind)))
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, kind) in found {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let is_dir = if kind.is_symlink() {
                let base = match &base {
                    Some(base) => base,
                    None => match dir.canonicalize() {
                        Ok(resolved) => base.insert(resolved),
                        Err(_) => return Vec::new(),
                    },
                };
                let Ok(resolved) = path.canonicalize() else {
                    continue;
                };
                if !resolved.starts_with(base) {
                    continue;
                }
                resolved.is_dir()
            } else {
                kind.is_dir()
            };
            if is_dir {
                if depth + 1 < MAX_DEPTH {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if path.file_name() == Some(std::ffi::OsStr::new("SKILL.md")) && current == dir {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(dir) {
                out.push(rel.to_string_lossy().into_owned());
            }
            if out.len() >= MAX_RESOURCES {
                out.sort();
                return out;
            }
        }
    }
    out.sort();
    out
}

/// Resolve `rel` inside `dir`, refusing anything that leaves it.
///
/// The workspace's own `Workspace::resolve` cannot do this job: it is
/// rooted at the workspace, and a skill directory may live under `$HOME`.
/// Rejecting `..` and absolute paths handles the lexical half;
/// canonicalizing both sides and comparing handles the symlink half,
/// which the lexical check alone leaves wide open.
fn contain(dir: &Path, rel: &str) -> Result<PathBuf, ToolError> {
    let candidate = Path::new(rel);
    if candidate.is_absolute() {
        return Err(ToolError::msg(format!(
            "`resource` must be relative to the skill folder, got: {rel}"
        )));
    }
    for component in candidate.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(ToolError::msg(format!(
                    "`resource` must stay inside the skill folder: {rel}"
                )))
            }
        }
    }
    let base = dir
        .canonicalize()
        .map_err(|e| ToolError::msg(format!("cannot read the skill folder: {e}")))?;
    let full = dir
        .join(candidate)
        .canonicalize()
        .map_err(|e| ToolError::msg(format!("cannot read {rel}: {e}")))?;
    if !full.starts_with(&base) {
        return Err(ToolError::msg(format!(
            "`resource` must stay inside the skill folder: {rel}"
        )));
    }
    Ok(full)
}

#[cfg(test)]
mod tests;
