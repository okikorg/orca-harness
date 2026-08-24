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
fn resources_of(dir: &Path) -> Vec<String> {
    const MAX_DEPTH: usize = 3;
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((current, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
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
mod tests {
    use super::*;
    use std::fs as stdfs;

    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "orca-skilltool-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = stdfs::remove_dir_all(&dir);
            stdfs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, body: &str) -> PathBuf {
            let path = self.0.join(rel);
            stdfs::create_dir_all(path.parent().unwrap()).unwrap();
            stdfs::write(&path, body).unwrap();
            path
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = stdfs::remove_dir_all(&self.0);
        }
    }

    fn ctx() -> ToolContext {
        ToolContext {
            call_id: "1".into(),
            tool_name: "skill".into(),
            cancellation: orca_harness_core::CancellationToken::new(),
            deadline: None,
        }
    }

    fn tool(temp: &Temp) -> SkillTool {
        let found = crate::skills::skill::discover(&crate::skills::skill::roots(
            &temp.0.join("repo"),
            None,
            None,
        ));
        assert!(found.failures.is_empty(), "{:?}", found.failures);
        SkillTool::new(found.skills)
    }

    fn release(temp: &Temp) {
        temp.write(
            "repo/.orca/skills/release/SKILL.md",
            "---\nname: release\ndescription: Cut a release\n---\n\n1. bump\n2. tag\n",
        );
        temp.write("repo/.orca/skills/release/checklist.md", "- green CI\n");
    }

    #[tokio::test]
    async fn loads_instructions_without_the_frontmatter() {
        let temp = Temp::new("load");
        release(&temp);
        let out = tool(&temp)
            .call(json!({"name": "release"}), &ctx())
            .await
            .unwrap();
        assert_eq!(out["instructions"], "1. bump\n2. tag\n");
        assert_eq!(out["name"], "release");
        assert_eq!(out["root"], ".orca/skills");
        assert_eq!(out["resources"], json!(["checklist.md"]));
        assert!(out.get("nextOffset").is_none());
    }

    #[tokio::test]
    async fn reads_a_resource_beside_the_instructions() {
        let temp = Temp::new("resource");
        release(&temp);
        let out = tool(&temp)
            .call(
                json!({"name": "release", "resource": "checklist.md"}),
                &ctx(),
            )
            .await
            .unwrap();
        assert_eq!(out["instructions"], "- green CI\n");
        assert_eq!(out["resource"], "checklist.md");
        // A resource is returned whole, frontmatter rules do not apply.
        assert!(out.get("resources").is_none());
    }

    /// The containment invariant: neither `..`, nor an absolute path,
    /// nor a symlink pointing out of the folder may escape it.
    #[tokio::test]
    async fn resource_cannot_escape_the_skill_folder() {
        let temp = Temp::new("escape");
        release(&temp);
        temp.write("secret.txt", "token\n");
        let skill = tool(&temp);

        for rel in ["../secret.txt", "../../secret.txt", "/etc/hosts"] {
            let err = skill
                .call(json!({"name": "release", "resource": rel}), &ctx())
                .await
                .expect_err("must refuse");
            assert!(err.to_string().contains("skill folder"), "rel {rel}: {err}");
        }

        #[cfg(unix)]
        {
            let link = temp.0.join("repo/.orca/skills/release/out.txt");
            std::os::unix::fs::symlink(temp.0.join("secret.txt"), &link).unwrap();
            let err = skill
                .call(json!({"name": "release", "resource": "out.txt"}), &ctx())
                .await
                .expect_err("a symlink out of the folder is still out of the folder");
            assert!(err.to_string().contains("skill folder"), "{err}");
        }
    }

    #[tokio::test]
    async fn long_instructions_page_with_next_offset() {
        let temp = Temp::new("paging");
        let body = "x".repeat(25);
        temp.write(
            "repo/.orca/skills/long/SKILL.md",
            &format!("---\nname: long\ndescription: long one\n---\n{body}"),
        );
        let skill = tool(&temp).chunk(10);

        let first = skill.call(json!({"name": "long"}), &ctx()).await.unwrap();
        assert_eq!(first["instructions"], "x".repeat(10));
        assert_eq!(first["nextOffset"], 10);

        let last = skill
            .call(json!({"name": "long", "offset": 20}), &ctx())
            .await
            .unwrap();
        assert_eq!(last["instructions"], "x".repeat(5));
        assert!(last.get("nextOffset").is_none());

        // Past the end is empty, not an error: a model that pages once
        // too often gets a clean stop.
        let past = skill
            .call(json!({"name": "long", "offset": 999}), &ctx())
            .await
            .unwrap();
        assert_eq!(past["instructions"], "");
    }

    #[tokio::test]
    async fn unknown_name_names_the_alternatives() {
        let temp = Temp::new("unknown");
        release(&temp);
        let err = tool(&temp)
            .call(json!({"name": "nope"}), &ctx())
            .await
            .expect_err("unknown skill");
        assert!(err.to_string().contains("available: release"), "{err}");
    }

    /// The body is read per call, so editing a skill mid-session works
    /// without a reload; only the catalog needs one.
    #[tokio::test]
    async fn body_reflects_an_edit_made_after_discovery() {
        let temp = Temp::new("edit");
        release(&temp);
        let skill = tool(&temp);
        temp.write(
            "repo/.orca/skills/release/SKILL.md",
            "---\nname: release\ndescription: Cut a release\n---\nrewritten\n",
        );
        let out = skill
            .call(json!({"name": "release"}), &ctx())
            .await
            .unwrap();
        assert_eq!(out["instructions"], "rewritten\n");
    }

    /// The catalog rides in the schema on every turn, and a user's whole
    /// `~/.claude/skills` collection can land in it at once, so each
    /// entry is bounded. The full description is one call away.
    #[test]
    fn catalog_entries_are_clipped() {
        let temp = Temp::new("clip");
        let long = "word ".repeat(80);
        temp.write(
            "repo/.orca/skills/verbose/SKILL.md",
            &format!("---\nname: verbose\ndescription: {long}\n---\nbody\n"),
        );
        let description = tool(&temp).schema().description;
        let line = description
            .lines()
            .find(|line| line.trim_start().starts_with("verbose"))
            .expect("catalog line");
        assert!(line.ends_with('…'), "{line}");
        assert!(line.chars().count() < 200, "{}", line.chars().count());
    }

    #[test]
    fn schema_carries_the_catalog_and_the_enum() {
        let temp = Temp::new("schema");
        release(&temp);
        let schema = tool(&temp).schema();
        assert_eq!(schema.name, "skill");
        assert!(schema.description.contains("release — Cut a release"));
        assert_eq!(
            schema.parameters["properties"]["name"]["enum"],
            json!(["release"])
        );
    }
}
