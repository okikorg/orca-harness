//! Filesystem administration tools: `copy_file`, `rename_file`,
//! `delete_file`, `create_folder`, `file_info`.
//!
//! Deliberately NOT in [`core_tools`](crate::core_tools): `shell` covers
//! all of this on a full host. They exist for restricted agents that run
//! without a shell — pair with a `ToolPolicy` that gates the destructive
//! ones. All paths are workspace-rooted like the other file tools.

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

/// `copy_file` — copy a single file (not a directory tree).
pub struct CopyFileTool {
    ws: Workspace,
}

impl CopyFileTool {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for CopyFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "copy_file".into(),
            description: "Copy a single workspace file to a new path, creating parent \
                directories. For directory trees use the shell."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "from": {"type": "string", "description": "Workspace-relative source file."},
                    "to": {"type": "string", "description": "Workspace-relative destination."}
                },
                "required": ["from", "to"]
            }),
        }
    }

    fn concurrency(&self, input: &Value) -> Concurrency {
        match input.get("to").and_then(Value::as_str) {
            Some(p) => Concurrency::Keyed(format!("file:{p}")),
            None => Concurrency::Serial,
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let from_rel = rel_arg(&input, "from")?;
        let to_rel = rel_arg(&input, "to")?;
        let from = self.ws.resolve(from_rel)?;
        let to = self.ws.resolve(to_rel)?;
        let meta = fs::metadata(&from)
            .await
            .map_err(|e| ToolError::msg(format!("source not readable: {e}")))?;
        if meta.is_dir() {
            return Err(ToolError::msg(
                "source is a directory; copy_file handles single files only",
            ));
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::msg(format!("mkdir failed: {e}")))?;
        }
        let bytes = fs::copy(&from, &to)
            .await
            .map_err(|e| ToolError::msg(format!("copy failed: {e}")))?;
        Ok(json!({ "from": from_rel, "to": to_rel, "bytesCopied": bytes }))
    }
}

/// `rename_file` — move/rename a file or directory.
pub struct RenameFileTool {
    ws: Workspace,
}

impl RenameFileTool {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for RenameFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "rename_file".into(),
            description: "Move or rename a workspace file or directory, creating parent \
                directories of the destination."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "from": {"type": "string", "description": "Workspace-relative source."},
                    "to": {"type": "string", "description": "Workspace-relative destination."}
                },
                "required": ["from", "to"]
            }),
        }
    }

    fn concurrency(&self, input: &Value) -> Concurrency {
        // Mutates two paths at once: key on both so it serializes against
        // writes to either path without excluding the whole batch.
        match (
            input.get("from").and_then(Value::as_str),
            input.get("to").and_then(Value::as_str),
        ) {
            (Some(from), Some(to)) => {
                Concurrency::Keys(vec![format!("file:{from}"), format!("file:{to}")])
            }
            _ => Concurrency::Serial,
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let from_rel = rel_arg(&input, "from")?;
        let to_rel = rel_arg(&input, "to")?;
        let from = self.ws.resolve(from_rel)?;
        let to = self.ws.resolve(to_rel)?;
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::msg(format!("mkdir failed: {e}")))?;
        }
        fs::rename(&from, &to)
            .await
            .map_err(|e| ToolError::msg(format!("rename failed: {e}")))?;
        Ok(json!({ "from": from_rel, "to": to_rel }))
    }
}

/// `delete_file` — remove a file, or a directory with `recursive`.
pub struct DeleteFileTool {
    ws: Workspace,
}

impl DeleteFileTool {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for DeleteFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "delete_file".into(),
            description: "Delete a workspace file. Directories require `recursive: true` \
                (empty or not)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative path."},
                    "recursive": {"type": "boolean", "default": false}
                },
                "required": ["path"]
            }),
        }
    }

    fn concurrency(&self, input: &Value) -> Concurrency {
        // A recursive delete touches an unknown set of paths.
        match (
            input
                .get("recursive")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            input.get("path").and_then(Value::as_str),
        ) {
            (false, Some(p)) => Concurrency::Keyed(format!("file:{p}")),
            _ => Concurrency::Serial,
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let rel = rel_arg(&input, "path")?;
        let recursive = input
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let path = self.ws.resolve(rel)?;
        let meta = fs::symlink_metadata(&path)
            .await
            .map_err(|e| ToolError::msg(format!("not found: {e}")))?;
        if meta.is_dir() {
            if !recursive {
                return Err(ToolError::msg(
                    "path is a directory; pass recursive: true to delete it",
                ));
            }
            fs::remove_dir_all(&path)
                .await
                .map_err(|e| ToolError::msg(format!("delete failed: {e}")))?;
        } else {
            fs::remove_file(&path)
                .await
                .map_err(|e| ToolError::msg(format!("delete failed: {e}")))?;
        }
        Ok(json!({ "path": rel, "deleted": true }))
    }
}

/// `create_folder` — create a directory and its parents.
pub struct CreateFolderTool {
    ws: Workspace,
}

impl CreateFolderTool {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for CreateFolderTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "create_folder".into(),
            description: "Create a workspace directory, including parents. Succeeds if it \
                already exists."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": {"type": "string", "description": "Workspace-relative path."} },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let rel = rel_arg(&input, "path")?;
        let path = self.ws.resolve(rel)?;
        fs::create_dir_all(&path)
            .await
            .map_err(|e| ToolError::msg(format!("mkdir failed: {e}")))?;
        Ok(json!({ "path": rel, "created": true }))
    }
}

/// `file_info` — metadata for a path without reading its contents.
pub struct FileInfoTool {
    ws: Workspace,
}

impl FileInfoTool {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for FileInfoTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "file_info".into(),
            description: "Metadata for a workspace path: existence, kind, size, and \
                modification time, without reading contents."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": {"type": "string", "description": "Workspace-relative path."} },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let rel = rel_arg(&input, "path")?;
        let path = self.ws.resolve(rel)?;
        let meta = match fs::symlink_metadata(&path).await {
            Ok(m) => m,
            Err(_) => return Ok(json!({ "path": rel, "exists": false })),
        };
        let kind = if meta.is_dir() {
            "dir"
        } else if meta.is_symlink() {
            "symlink"
        } else {
            "file"
        };
        let modified_epoch_secs = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        Ok(json!({
            "path": rel,
            "exists": true,
            "kind": kind,
            "sizeBytes": meta.len(),
            "readonly": meta.permissions().readonly(),
            "modifiedEpochSecs": modified_epoch_secs,
        }))
    }
}
