//! Filesystem tools, all rooted at a [`Workspace`]. Reads are `Parallel`;
//! writes and edits are `Keyed` by target path, so two writes to the same
//! file serialize while writes to different files run concurrently — the
//! exact guarantee the kernel's keyed dispatch provides.
//!
//! A [`FileGuard`] shared between the read, write, and edit tools turns
//! `write_file` into read-before-write: see its documentation for what
//! that costs and what it buys.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use crate::workspace::Workspace;

/// What a file looked like the last time these tools saw it. Modified
/// time and length together are enough to notice an outside edit without
/// hashing the contents on every call; a change that preserves both is
/// possible in theory and has never been the failure this guards against
/// (an editor saving, a `git checkout`, a build regenerating a file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        }
    }
}

/// Read-before-write bookkeeping, shared by `read_file`, `write_file`,
/// and `edit_file`.
///
/// `write_file` replaces a file wholesale, so a model that has not seen
/// the current contents is not overwriting a file — it is deleting one
/// and writing another. The guard refuses that: an existing file may be
/// overwritten only if these tools read it (or wrote it) and nothing has
/// changed it since. Creating a new file needs no prior read, and
/// `edit_file` is exempt from the check because it works from the
/// contents it just read and fails when its `old` text is not there.
///
/// The guard is scoped by whoever holds it. One guard per agent session
/// is the useful lifetime; hosts that rebuild their tool set mid-session
/// should keep the guard across rebuilds
/// ([`core_tools_with_guard`](crate::core_tools_with_guard)) and
/// [`clear`](FileGuard::clear) it when the conversation resets — the
/// model that did the reading is gone by then.
#[derive(Clone, Default)]
pub struct FileGuard(Arc<Mutex<HashMap<PathBuf, Stamp>>>);

impl FileGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget every recorded read, so the next overwrite of an existing
    /// file has to read it again.
    pub fn clear(&self) {
        self.0.lock().expect("file guard lock").clear();
    }

    /// How many files are currently stamped.
    pub fn len(&self) -> usize {
        self.0.lock().expect("file guard lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn record(&self, path: &Path, stamp: Stamp) {
        self.0
            .lock()
            .expect("file guard lock")
            .insert(path.to_path_buf(), stamp);
    }

    fn stamp(&self, path: &Path) -> Option<Stamp> {
        self.0.lock().expect("file guard lock").get(path).copied()
    }

    /// Stamp what is on disk now. Called after a write, from the file's
    /// own post-write metadata rather than the clock, so the stamp is
    /// exactly what the next check will compare against.
    async fn restamp(&self, path: &Path) {
        if let Ok(meta) = fs::metadata(path).await {
            self.record(path, Stamp::of(&meta));
        }
    }

    /// The read-before-write check. `Ok` when the path does not exist
    /// (a create), when it is not a regular file (the write will fail on
    /// its own terms), or when the recorded stamp still matches.
    async fn check_overwrite(&self, path: &Path, rel: &str) -> Result<(), ToolError> {
        let Ok(meta) = fs::metadata(path).await else {
            return Ok(());
        };
        if !meta.is_file() {
            return Ok(());
        }
        match self.stamp(path) {
            Some(seen) if seen == Stamp::of(&meta) => Ok(()),
            Some(_) => Err(ToolError::msg(format!(
                "{rel} changed on disk after you read it — read_file it again before \
                 overwriting, or use edit_file, which works from the current contents"
            ))),
            None => Err(ToolError::msg(format!(
                "{rel} already exists and has not been read — read_file it first so the \
                 overwrite is deliberate, or use edit_file to change part of it"
            ))),
        }
    }
}

fn rel_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::msg(format!("`{key}` (string) is required")))
}

/// Move a string argument out of the input (leaving `null` behind).
/// Owned strings hit tokio's zero-copy `fs::write` fast path; a borrowed
/// `&str` would be deep-copied to a fresh buffer on every call.
fn take_string_arg(input: &mut Value, key: &str) -> Result<String, ToolError> {
    match input.get_mut(key).map(Value::take) {
        Some(Value::String(s)) => Ok(s),
        _ => Err(ToolError::msg(format!("`{key}` (string) is required"))),
    }
}

/// `read_file` — return a text file's contents.
pub struct ReadFileTool {
    ws: Workspace,
    max_bytes: usize,
    guard: Option<FileGuard>,
}

impl ReadFileTool {
    pub fn new(ws: Workspace) -> Self {
        Self {
            ws,
            max_bytes: 256 * 1024,
            guard: None,
        }
    }
    pub fn max_bytes(mut self, bytes: usize) -> Self {
        self.max_bytes = bytes;
        self
    }

    /// Record what this tool reads, so a later `write_file` through the
    /// same [`FileGuard`] can tell the model saw the file first.
    pub fn guard(mut self, guard: FileGuard) -> Self {
        self.guard = Some(guard);
        self
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_file".into(),
            description: "Read a UTF-8 text file relative to the workspace root.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": {"type": "string", "description": "Workspace-relative path."} },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let path = self.ws.resolve(rel_arg(&input, "path")?)?;
        let _io = crate::iogate::fs_permit().await;
        // Stamp from before the read, not after: if something rewrites
        // the file while it is being read, the stamp stays older than
        // the contents and the next overwrite is refused. The other
        // order would silently bless a write based on stale contents.
        let stamp = match &self.guard {
            Some(_) => fs::metadata(&path).await.ok().map(|meta| Stamp::of(&meta)),
            None => None,
        };
        let bytes = fs::read(&path)
            .await
            .map_err(|e| ToolError::msg(format!("read failed: {e}")))?;
        if let (Some(guard), Some(stamp)) = (&self.guard, stamp) {
            guard.record(&path, stamp);
        }
        let truncated = bytes.len() > self.max_bytes;
        let slice = if truncated {
            &bytes[..self.max_bytes]
        } else {
            &bytes[..]
        };
        Ok(json!({
            "content": String::from_utf8_lossy(slice),
            "bytes": bytes.len(),
            "truncated": truncated,
        }))
    }
}

/// `write_file` — create or overwrite a file, creating parent dirs.
pub struct WriteFileTool {
    ws: Workspace,
    guard: Option<FileGuard>,
}

impl WriteFileTool {
    pub fn new(ws: Workspace) -> Self {
        Self { ws, guard: None }
    }

    /// Require that an existing file was read (through the same
    /// [`FileGuard`]) and is unchanged before it may be overwritten.
    pub fn guard(mut self, guard: FileGuard) -> Self {
        self.guard = Some(guard);
        self
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "write_file".into(),
            description: "Create a workspace file, or overwrite one you have already read in \
                this session, with the given contents. To change part of an existing file, \
                prefer edit_file."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative path."},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn concurrency(&self, input: &Value) -> Concurrency {
        match input.get("path").and_then(Value::as_str) {
            Some(p) => Concurrency::Keyed(format!("file:{p}")),
            None => Concurrency::Serial,
        }
    }

    async fn call(&self, mut input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let rel = rel_arg(&input, "path")?.to_owned();
        let content = take_string_arg(&mut input, "content")?;
        let bytes = content.len();
        let path = self.ws.resolve(&rel)?;
        let _io = crate::iogate::fs_permit().await;
        // Under the keyed lock for this path, so the check and the write
        // cannot be separated by another call to the same file.
        if let Some(guard) = &self.guard {
            guard.check_overwrite(&path, &rel).await?;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::msg(format!("mkdir failed: {e}")))?;
        }
        fs::write(&path, content)
            .await
            .map_err(|e| ToolError::msg(format!("write failed: {e}")))?;
        if let Some(guard) = &self.guard {
            guard.restamp(&path).await;
        }
        Ok(json!({ "path": rel, "bytesWritten": bytes }))
    }
}

/// `edit_file` — replace an exact substring, once or for all occurrences.
/// Fails if `old` is absent or (for single mode) not unique.
pub struct EditFileTool {
    ws: Workspace,
    guard: Option<FileGuard>,
}

impl EditFileTool {
    pub fn new(ws: Workspace) -> Self {
        Self { ws, guard: None }
    }

    /// Stamp what this tool writes. An edit is never *checked* against
    /// the guard — it reads the current contents and fails if its `old`
    /// text is not in them, which is the same protection by other means
    /// — but its write moves the file's mtime, and leaving that
    /// unrecorded would strand the next `write_file` to the same path.
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
            description: "Replace an exact string in a workspace file. By default the match \
                must be unique; set replaceAll to replace every occurrence."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old": {"type": "string", "description": "Exact text to replace."},
                    "new": {"type": "string", "description": "Replacement text."},
                    "replaceAll": {"type": "boolean", "default": false}
                },
                "required": ["path", "old", "new"]
            }),
        }
    }

    fn concurrency(&self, input: &Value) -> Concurrency {
        match input.get("path").and_then(Value::as_str) {
            Some(p) => Concurrency::Keyed(format!("file:{p}")),
            None => Concurrency::Serial,
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let rel = rel_arg(&input, "path")?;
        let old = rel_arg(&input, "old")?;
        let new = rel_arg(&input, "new")?;
        let replace_all = input
            .get("replaceAll")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if old.is_empty() {
            return Err(ToolError::msg("`old` must not be empty"));
        }
        let path = self.ws.resolve(rel)?;
        let _io = crate::iogate::fs_permit().await;
        let content = fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::msg(format!("read failed: {e}")))?;

        let count = content.matches(old).count();
        if count == 0 {
            return Err(ToolError::msg("`old` not found in file"));
        }
        if count > 1 && !replace_all {
            return Err(ToolError::msg(format!(
                "`old` occurs {count} times; pass replaceAll or make it unique"
            )));
        }
        let updated = if replace_all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        };
        // Owned String: tokio's zero-copy fast path (a borrow would be
        // deep-copied onto the blocking pool).
        fs::write(&path, updated)
            .await
            .map_err(|e| ToolError::msg(format!("write failed: {e}")))?;
        if let Some(guard) = &self.guard {
            guard.restamp(&path).await;
        }
        Ok(json!({ "path": rel, "replacements": if replace_all { count } else { 1 } }))
    }
}

/// `list_dir` — list entries of a workspace directory (non-recursive).
pub struct ListDirTool {
    ws: Workspace,
}

impl ListDirTool {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "list_dir".into(),
            description: "List the entries of a workspace directory (non-recursive).".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": {"type": "string", "default": "."} },
            }),
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let rel = input.get("path").and_then(Value::as_str).unwrap_or(".");
        let dir = self.ws.resolve(rel)?;
        let _io = crate::iogate::fs_permit().await;
        let mut entries = fs::read_dir(&dir)
            .await
            .map_err(|e| ToolError::msg(format!("read_dir failed: {e}")))?;
        let mut out = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| ToolError::msg(format!("read_dir failed: {e}")))?
        {
            let file_type = entry.file_type().await.ok();
            out.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "isDir": file_type.map(|t| t.is_dir()).unwrap_or(false),
            }));
        }
        out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({ "path": rel, "entries": out }))
    }
}

// The test module sits at the end of the file: clippy's
// `items_after_test_module` treats anything below it as misplaced.
#[cfg(test)]
mod tests {
    use super::take_string_arg;
    use serde_json::json;

    /// The write path hands content to tokio's `fs::write`, whose
    /// zero-copy fast path needs an OWNED String — taking it out of the
    /// input must move the buffer, never copy it.
    #[test]
    fn take_string_arg_moves_the_buffer_out_of_the_input() {
        let mut input = json!({"content": "x".repeat(1024), "path": "a.txt"});
        let original_ptr = input["content"].as_str().unwrap().as_ptr();

        let taken = take_string_arg(&mut input, "content").unwrap();
        assert_eq!(taken.as_ptr(), original_ptr, "must move, not copy");
        assert!(input["content"].is_null(), "the input no longer owns it");
        assert_eq!(input["path"], "a.txt", "other fields untouched");
    }

    #[test]
    fn take_string_arg_rejects_missing_or_non_string() {
        assert!(take_string_arg(&mut json!({}), "content").is_err());
        assert!(take_string_arg(&mut json!({"content": 7}), "content").is_err());
    }
}
