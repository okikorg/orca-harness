//! Filesystem tools, all rooted at a [`Workspace`]. Reads are `Parallel`;
//! writes and edits are `Keyed` by target path, so two writes to the same
//! file serialize while writes to different files run concurrently — the
//! exact guarantee the kernel's keyed dispatch provides.

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use crate::workspace::Workspace;

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
}

impl ReadFileTool {
    pub fn new(ws: Workspace) -> Self {
        Self {
            ws,
            max_bytes: 256 * 1024,
        }
    }
    pub fn max_bytes(mut self, bytes: usize) -> Self {
        self.max_bytes = bytes;
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
        let bytes = fs::read(&path)
            .await
            .map_err(|e| ToolError::msg(format!("read failed: {e}")))?;
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
}

impl WriteFileTool {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "write_file".into(),
            description: "Create or overwrite a workspace file with the given contents.".into(),
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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::msg(format!("mkdir failed: {e}")))?;
        }
        fs::write(&path, content)
            .await
            .map_err(|e| ToolError::msg(format!("write failed: {e}")))?;
        Ok(json!({ "path": rel, "bytesWritten": bytes }))
    }
}

/// `edit_file` — replace an exact substring, once or for all occurrences.
/// Fails if `old` is absent or (for single mode) not unique.
pub struct EditFileTool {
    ws: Workspace,
}

impl EditFileTool {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
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
        Ok(json!({ "path": rel, "replacements": if replace_all { count } else { 1 } }))
    }
}

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
