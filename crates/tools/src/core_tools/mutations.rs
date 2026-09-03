//! Transactional multi-file mutation tools.
//!
//! `multi_edit` applies ordered exact-string replacements. `apply_patch`
//! accepts the compact `*** Begin Patch` format used by coding agents. Both
//! preflight every operation before the first write and declare every touched
//! path through [`Concurrency::Keys`], so overlapping calls serialize while
//! disjoint patches can run together.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::{fs, task::JoinSet};

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use crate::{FileGuard, Workspace};

use super::multi_edit_spec::{self, EditOperation, MAX_OPERATIONS};
use super::patch_format::{apply_hunks, parse_patch, PatchAction};

#[derive(Clone)]
struct Change {
    rel: String,
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}

async fn apply_state(change: &Change, after: bool) -> Result<(), std::io::Error> {
    let _io = crate::iogate::fs_permit().await;
    let state = if after { &change.after } else { &change.before };
    match state {
        Some(content) => {
            if let Some(parent) = change.path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&change.path, content).await
        }
        None => match fs::remove_file(&change.path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
    }
}

/// Commit preflighted changes and restore already-written paths if a later
/// filesystem operation fails. Match/path errors never reach this phase.
async fn commit(changes: &[Change]) -> Result<(), ToolError> {
    for (index, change) in changes.iter().enumerate() {
        if let Err(error) = apply_state(change, true).await {
            let mut rollback_errors = Vec::new();
            // Include the failing change: a failed write may already have
            // created or truncated its destination.
            for previous in changes[..=index].iter().rev() {
                if let Err(rollback) = apply_state(previous, false).await {
                    rollback_errors.push(format!("{}: {rollback}", previous.rel));
                }
            }
            let suffix = if rollback_errors.is_empty() {
                String::new()
            } else {
                format!("; rollback also failed for {}", rollback_errors.join(", "))
            };
            return Err(ToolError::msg(format!(
                "could not apply {}: {error}{suffix}",
                change.rel
            )));
        }
    }
    Ok(())
}

fn keyed_paths<'a>(ws: &Workspace, paths: impl IntoIterator<Item = &'a str>) -> Concurrency {
    let mut keys = BTreeSet::new();
    for rel in paths {
        let Ok((rel, _)) = normalized_path(ws, rel) else {
            // Invalid inputs fail in `call`. Until then, serialize them so a
            // malformed alias can never bypass a valid path's keyed lock.
            return Concurrency::Serial;
        };
        keys.insert(format!("file:{rel}"));
    }
    if keys.is_empty() {
        Concurrency::Serial
    } else {
        Concurrency::Keys(keys.into_iter().collect())
    }
}

fn normalized_path(ws: &Workspace, rel: &str) -> Result<(String, PathBuf), ToolError> {
    let path = ws.resolve(rel)?;
    let rel = ws.display_rel(&path).to_string_lossy().into_owned();
    Ok((rel, path))
}

/// Read all preflight snapshots concurrently. Each individual read passes
/// through the process-wide filesystem gate; parsing and matching do not hold
/// a permit while other calls are waiting to perform actual I/O.
async fn read_snapshots(
    paths: &BTreeMap<String, PathBuf>,
) -> Result<BTreeMap<String, Option<Vec<u8>>>, ToolError> {
    if let Some((rel, path)) = paths.first_key_value().filter(|_| paths.len() == 1) {
        let _io = crate::iogate::fs_permit().await;
        let snapshot = match fs::read(path).await {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(ToolError::msg(format!("read {rel} failed: {error}")));
            }
        };
        return Ok(BTreeMap::from([(rel.clone(), snapshot)]));
    }

    let mut reads = JoinSet::new();
    for (rel, path) in paths {
        let rel = rel.clone();
        let path = path.clone();
        reads.spawn(async move {
            let _io = crate::iogate::fs_permit().await;
            let snapshot = match fs::read(path).await {
                Ok(content) => Some(content),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(ToolError::msg(format!("read {rel} failed: {error}"))),
            };
            Ok::<_, ToolError>((rel, snapshot))
        });
    }

    let mut snapshots = BTreeMap::new();
    while let Some(result) = reads.join_next().await {
        let (rel, snapshot) = result
            .map_err(|error| ToolError::msg(format!("preflight read task failed: {error}")))??;
        snapshots.insert(rel, snapshot);
    }
    Ok(snapshots)
}

fn replace_exact(
    content: &mut String,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<usize, usize> {
    if replace_all {
        if old == new {
            let replacements = content.matches(old).count();
            return (replacements > 0).then_some(replacements).ok_or(0);
        }
        let mut replacements = 0usize;
        let mut cursor = 0usize;
        let mut updated = String::with_capacity(content.len());
        for (start, matched) in content.match_indices(old) {
            updated.push_str(&content[cursor..start]);
            updated.push_str(new);
            cursor = start + matched.len();
            replacements += 1;
        }
        if replacements == 0 {
            return Err(0);
        }
        updated.push_str(&content[cursor..]);
        *content = updated;
        return Ok(replacements);
    }

    let Some(start) = content.find(old) else {
        return Err(0);
    };
    let end = start + old.len();
    if let Some(second) = content[end..].find(old) {
        let remaining = &content[end + second + old.len()..];
        return Err(2 + remaining.matches(old).count());
    }
    if old != new {
        content.replace_range(start..end, new);
    }
    Ok(1)
}

/// Apply several exact replacements or appends in one reviewed tool call.
pub struct MultiEditTool {
    ws: Workspace,
    guard: Option<FileGuard>,
}

impl MultiEditTool {
    pub fn new(ws: Workspace) -> Self {
        Self { ws, guard: None }
    }

    pub fn guard(mut self, guard: FileGuard) -> Self {
        self.guard = Some(guard);
        self
    }
}

#[async_trait]
impl Tool for MultiEditTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "multi_edit".into(),
            description: "Apply an ordered, transactional batch of exact replacements and appends across existing workspace files. All edits are validated before any file is written.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_OPERATIONS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "Workspace-relative path."},
                                "operation": {
                                    "type": "string",
                                    "enum": ["replace", "append"],
                                    "default": "replace",
                                    "description": "Use `replace` (default) for exact replacement or `append` to add content at EOF."
                                },
                                "old": {"type": "string", "description": "Exact text to replace."},
                                "new": {"type": "string", "description": "Replacement text."},
                                "replaceAll": {"type": "boolean", "default": false},
                                "content": {"type": "string", "description": "Text appended exactly at EOF when operation is `append`."}
                            },
                            "required": ["path"],
                            "oneOf": [
                                {"properties": {"operation": {"enum": ["replace"]}}, "required": ["old", "new"]},
                                {"properties": {"operation": {"const": "append"}}, "required": ["operation", "content"]}
                            ]
                        }
                    }
                },
                "required": ["edits"]
            }),
        }
    }

    fn concurrency(&self, input: &Value) -> Concurrency {
        match input.get("edits").and_then(Value::as_array) {
            Some(edits) => keyed_paths(
                &self.ws,
                edits
                    .iter()
                    .filter_map(|edit| edit.get("path").and_then(Value::as_str)),
            ),
            None => Concurrency::Serial,
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let mut specs = multi_edit_spec::parse(&input)?;
        let mut paths = BTreeMap::new();
        for spec in &mut specs {
            let (rel, path) = normalized_path(&self.ws, &spec.path)?;
            spec.path = rel.clone();
            paths.entry(rel).or_insert(path);
        }

        let mut contents = BTreeMap::new();
        let mut originals = BTreeMap::new();
        for (rel, snapshot) in read_snapshots(&paths).await? {
            let bytes = snapshot
                .ok_or_else(|| ToolError::msg(format!("read {rel} failed: file not found")))?;
            let content = String::from_utf8(bytes)
                .map_err(|error| ToolError::msg(format!("read {rel} failed: {error}")))?;
            originals.insert(rel.clone(), content.clone());
            contents.insert(rel.clone(), content);
        }

        let mut replacements = 0usize;
        let mut appends = 0usize;
        for (index, spec) in specs.iter().enumerate() {
            let content = contents.get_mut(&spec.path).expect("preloaded edit path");
            match &spec.operation {
                EditOperation::Replace {
                    old,
                    new,
                    replace_all,
                } => match replace_exact(content, old, new, *replace_all) {
                    Ok(count) => replacements += count,
                    Err(0) => {
                        return Err(ToolError::msg(format!(
                            "edit {index} for {}: `old` not found",
                            spec.path
                        )))
                    }
                    Err(count) => {
                        return Err(ToolError::msg(format!(
                            "edit {index} for {}: `old` occurs {count} times ({}); set replaceAll or make it unique",
                            spec.path,
                            super::files::occurrence_lines(content, old)
                        )))
                    }
                },
                EditOperation::Append { content: appended } => {
                    content.push_str(appended);
                    appends += 1;
                }
            }
        }

        let changes: Vec<Change> = contents
            .into_iter()
            .filter_map(|(rel, after)| {
                let before = originals.remove(&rel).expect("original for edit path");
                (before != after).then(|| Change {
                    path: paths[&rel].clone(),
                    rel,
                    before: Some(before.into_bytes()),
                    after: Some(after.into_bytes()),
                })
            })
            .collect();
        commit(&changes).await?;
        if let Some(guard) = &self.guard {
            for change in &changes {
                let _io = crate::iogate::fs_permit().await;
                guard.restamp(&change.path).await;
            }
        }
        let paths: Vec<&str> = changes.iter().map(|change| change.rel.as_str()).collect();
        Ok(json!({
            "editsApplied": specs.len(),
            "filesChanged": changes.len(),
            "replacements": replacements,
            "appends": appends,
            "paths": paths,
        }))
    }
}

fn patch_concurrency(ws: &Workspace, input: &Value) -> Concurrency {
    let Some(patch) = input.get("patch").and_then(Value::as_str) else {
        return Concurrency::Serial;
    };
    // Classification only needs the file headers. Full syntax and path
    // validation stays in `call`; parsing a large patch twice would add pure
    // overhead before the tool can begin.
    let paths: Vec<&str> = patch
        .lines()
        .filter_map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            line.strip_prefix("*** Add File: ")
                .or_else(|| line.strip_prefix("*** Update File: "))
                .or_else(|| line.strip_prefix("*** Delete File: "))
        })
        .collect();
    keyed_paths(ws, paths)
}

/// Apply a multi-file coding patch without invoking a shell command.
pub struct ApplyPatchTool {
    ws: Workspace,
    guard: Option<FileGuard>,
}

impl ApplyPatchTool {
    pub fn new(ws: Workspace) -> Self {
        Self { ws, guard: None }
    }

    pub fn guard(mut self, guard: FileGuard) -> Self {
        self.guard = Some(guard);
        self
    }
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "apply_patch".into(),
            description: "Apply one validated multi-file patch using `*** Begin Patch` with `*** Add File`, `*** Update File`, and `*** Delete File` sections. Update hunks start with `@@`; context, added, and removed lines start with space, `+`, and `-`. Hunks apply in order: a file's first hunk must match uniquely, and each later hunk takes the nearest match after the previous one. A hunk with only context lines is a position anchor: it changes nothing and scopes the next hunk to after its first match.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": {"type": "string", "description": "The complete patch text."}
                },
                "required": ["patch"]
            }),
        }
    }

    fn concurrency(&self, input: &Value) -> Concurrency {
        patch_concurrency(&self.ws, input)
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let patch = input
            .get("patch")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`patch` (string) is required"))?;
        let actions = parse_patch(patch)?;
        let mut resolved = BTreeMap::new();
        let mut resolved_paths = BTreeMap::new();
        for action in &actions {
            let (normalized, path) = normalized_path(&self.ws, action.path())?;
            if let Some(previous) = resolved_paths.insert(path.clone(), action.path()) {
                return Err(ToolError::msg(format!(
                    "patch operations `{previous}` and `{}` resolve to the same path `{normalized}`",
                    action.path()
                )));
            }
            resolved.insert(action.path().to_owned(), path);
        }

        let mut snapshots = read_snapshots(&resolved).await?;
        let mut changes = Vec::with_capacity(actions.len());
        for action in actions {
            let rel = action.path().to_owned();
            let path = resolved[&rel].clone();
            let snapshot = snapshots
                .remove(&rel)
                .expect("preflight snapshot for patch path");
            match action {
                PatchAction::Add { lines, .. } => {
                    if snapshot.is_some() {
                        return Err(ToolError::msg(format!(
                            "cannot add {rel}: path already exists"
                        )));
                    }
                    let mut content = lines.join("\n");
                    if !lines.is_empty() {
                        content.push('\n');
                    }
                    changes.push(Change {
                        rel,
                        path,
                        before: None,
                        after: Some(content.into_bytes()),
                    });
                }
                PatchAction::Update { hunks, .. } => {
                    let before = snapshot.ok_or_else(|| {
                        ToolError::msg(format!("read {rel} failed: file not found"))
                    })?;
                    let before = String::from_utf8(before)
                        .map_err(|error| ToolError::msg(format!("read {rel} failed: {error}")))?;
                    let after = apply_hunks(&rel, &before, &hunks)?;
                    if before != after {
                        changes.push(Change {
                            rel,
                            path,
                            before: Some(before.into_bytes()),
                            after: Some(after.into_bytes()),
                        });
                    }
                }
                PatchAction::Delete { .. } => {
                    let before = snapshot.ok_or_else(|| {
                        ToolError::msg(format!("read {rel} failed: file not found"))
                    })?;
                    changes.push(Change {
                        rel,
                        path,
                        before: Some(before),
                        after: None,
                    });
                }
            }
        }
        commit(&changes).await?;
        if let Some(guard) = &self.guard {
            for change in &changes {
                let _io = crate::iogate::fs_permit().await;
                if change.after.is_some() {
                    guard.restamp(&change.path).await;
                } else {
                    guard.forget(&change.path);
                }
            }
        }
        let added = changes
            .iter()
            .filter(|change| change.before.is_none())
            .count();
        let deleted = changes
            .iter()
            .filter(|change| change.after.is_none())
            .count();
        let paths: Vec<&str> = changes.iter().map(|change| change.rel.as_str()).collect();
        Ok(json!({
            "filesChanged": changes.len(),
            "added": added,
            "updated": changes.len() - added - deleted,
            "deleted": deleted,
            "paths": paths,
        }))
    }
}
