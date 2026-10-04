//! `edit_file` — transactional exact-string edits across one or more files.
//!
//! One call is either a single `{path, old, new}` edit or an ordered
//! `edits` batch. Every edit is matched before the first write, and every
//! touched path is declared through [`Concurrency::Keys`], so overlapping
//! calls serialize while disjoint edits can run together.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::task::JoinSet;

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use crate::{FileGuard, Workspace};

use super::edit_spec::{self, EditOperation, MAX_OPERATIONS};

struct Change {
    rel: String,
    path: PathBuf,
    before: String,
    after: String,
}

/// Commit preflighted changes and restore already-written paths if a later
/// filesystem operation fails. Match/path errors never reach this phase.
async fn commit(ws: &Workspace, changes: &[Change]) -> Result<(), ToolError> {
    for (index, change) in changes.iter().enumerate() {
        let written = {
            let _io = crate::iogate::fs_permit().await;
            ws.write(&change.path, change.after.as_bytes()).await
        };
        if let Err(error) = written {
            let mut rollback_errors = Vec::new();
            // Include the failing change: a failed write may already have
            // truncated its destination.
            for previous in changes[..=index].iter().rev() {
                let _io = crate::iogate::fs_permit().await;
                if let Err(rollback) = ws.write(&previous.path, previous.before.as_bytes()).await {
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

async fn read_text(
    ws: Workspace,
    rel: String,
    path: PathBuf,
) -> Result<(String, String), ToolError> {
    let _io = crate::iogate::fs_permit().await;
    let bytes = ws
        .read(&path)
        .await
        .map_err(|error| ToolError::msg(format!("read {rel} failed: {error}")))?;
    let content = String::from_utf8(bytes)
        .map_err(|_| ToolError::msg(format!("read {rel} failed: file is not valid UTF-8")))?;
    Ok((rel, content))
}

/// Read every edited file concurrently. Each read passes through the
/// process-wide filesystem gate; matching does not hold a permit while
/// other calls are waiting to perform actual I/O.
async fn read_all(
    ws: &Workspace,
    paths: &BTreeMap<String, PathBuf>,
) -> Result<BTreeMap<String, String>, ToolError> {
    if let Some((rel, path)) = paths.first_key_value().filter(|_| paths.len() == 1) {
        return Ok(BTreeMap::from([read_text(
            ws.clone(),
            rel.clone(),
            path.clone(),
        )
        .await?]));
    }
    let mut reads = JoinSet::new();
    for (rel, path) in paths {
        // Workspace is Arc-backed, so each task carries a handle to the
        // same backend rather than a copy of it.
        reads.spawn(read_text(ws.clone(), rel.clone(), path.clone()));
    }
    let mut contents = BTreeMap::new();
    while let Some(result) = reads.join_next().await {
        let (rel, content) = result
            .map_err(|error| ToolError::msg(format!("preflight read task failed: {error}")))??;
        contents.insert(rel, content);
    }
    Ok(contents)
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

/// Apply one exact replacement, or an ordered batch of replacements and
/// appends, in a single reviewed tool call.
pub struct EditFileTool {
    ws: Workspace,
    guard: Option<FileGuard>,
}

impl EditFileTool {
    pub fn new(ws: Workspace) -> Self {
        Self { ws, guard: None }
    }

    /// Restamp edited files in the shared [`FileGuard`], so a later
    /// `write_file` sees the edit as the content it last read.
    pub fn guard(mut self, guard: FileGuard) -> Self {
        self.guard = Some(guard);
        self
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "edit_file".into(),
            description: "Replace exact text in existing workspace files. Pass `path`, `old`, \
                and `new` for one edit, or `edits` for an ordered batch across files. `old` \
                must match exactly once unless replaceAll is set. A batch is transactional: \
                every edit is matched before any file is written."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative path."},
                    "old": {"type": "string", "description": "Exact text to replace."},
                    "new": {"type": "string", "description": "Replacement text."},
                    "replaceAll": {"type": "boolean", "default": false},
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_OPERATIONS,
                        "description": "Ordered batch; use instead of the top-level fields.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "Workspace-relative path."},
                                "operation": {
                                    "type": "string",
                                    "enum": ["replace", "append"],
                                    "default": "replace",
                                    "description": "`replace` (default) swaps `old` for `new`; `append` adds `content` at EOF."
                                },
                                "old": {"type": "string"},
                                "new": {"type": "string"},
                                "replaceAll": {"type": "boolean", "default": false},
                                "content": {"type": "string", "description": "Text appended at EOF when operation is `append`."}
                            },
                            "required": ["path"]
                        }
                    }
                }
            }),
        }
    }

    fn concurrency(&self, input: &Value) -> Concurrency {
        match edit_spec::edits(input) {
            Ok(edits) => keyed_paths(
                &self.ws,
                edits
                    .iter()
                    .filter_map(|edit| edit.get("path").and_then(Value::as_str)),
            ),
            Err(_) => Concurrency::Serial,
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let mut specs = edit_spec::parse(&input)?;
        let mut paths = BTreeMap::new();
        for spec in &mut specs {
            let (rel, path) = normalized_path(&self.ws, &spec.path)?;
            spec.path = rel.clone();
            paths.entry(rel).or_insert(path);
        }

        let originals = read_all(&self.ws, &paths).await?;
        let mut contents = originals.clone();
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

        let changes: Vec<Change> = originals
            .into_iter()
            .zip(contents.into_values())
            .filter(|((_, before), after)| before != after)
            .map(|((rel, before), after)| Change {
                path: paths[&rel].clone(),
                rel,
                before,
                after,
            })
            .collect();
        commit(&self.ws, &changes).await?;
        if let Some(guard) = &self.guard {
            for change in &changes {
                let _io = crate::iogate::fs_permit().await;
                guard.restamp(&self.ws, &change.path).await;
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
