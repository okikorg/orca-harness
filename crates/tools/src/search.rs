//! `grep` — recursive literal substring search across the workspace.
//!
//! Literal, not regex, to keep the dependency set conservative (the
//! architecture doc's guidance). A host that wants full regex can expose
//! ripgrep through the `shell` tool instead.

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use orca_harness_core::{Tool, ToolContext, ToolError, ToolSchema};

use crate::workspace::Workspace;

pub struct GrepTool {
    ws: Workspace,
    max_results: usize,
    max_file_bytes: usize,
}

impl GrepTool {
    pub fn new(ws: Workspace) -> Self {
        Self {
            ws,
            max_results: 200,
            max_file_bytes: 2 * 1024 * 1024,
        }
    }
    pub fn max_results(mut self, n: usize) -> Self {
        self.max_results = n;
        self
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "grep".into(),
            description: "Recursively search workspace files for a literal substring and \
                return matching lines with their file path and line number."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Literal substring to find."},
                    "path": {"type": "string", "default": ".", "description": "Subtree to search."}
                },
                "required": ["query"]
            }),
        }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`query` (string) is required"))?;
        if query.is_empty() {
            return Err(ToolError::msg("`query` must not be empty"));
        }
        let start_rel = input.get("path").and_then(Value::as_str).unwrap_or(".");
        let start = self.ws.resolve(start_rel)?;

        let mut matches = Vec::new();
        let mut stack = vec![start];
        let mut truncated = false;

        'outer: while let Some(dir) = stack.pop() {
            if ctx.cancellation.is_cancelled() {
                return Err(ToolError::msg("cancelled"));
            }
            let meta = match fs::metadata(&dir).await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                let mut rd = match fs::read_dir(&dir).await {
                    Ok(rd) => rd,
                    Err(_) => continue,
                };
                while let Ok(Some(entry)) = rd.next_entry().await {
                    let name = entry.file_name();
                    // Skip common noise directories.
                    if matches!(
                        name.to_str(),
                        Some(".git") | Some("target") | Some("node_modules")
                    ) {
                        continue;
                    }
                    stack.push(entry.path());
                }
            } else if meta.len() as usize <= self.max_file_bytes {
                let content = match fs::read(&dir).await {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                // Skip probable binaries.
                if content.contains(&0) {
                    continue;
                }
                let text = String::from_utf8_lossy(&content);
                let display = self.ws.display_rel(&dir).to_string_lossy().into_owned();
                for (lineno, line) in text.lines().enumerate() {
                    if line.contains(query) {
                        matches.push(json!({
                            "path": display,
                            "line": lineno + 1,
                            "text": line.chars().take(400).collect::<String>(),
                        }));
                        if matches.len() >= self.max_results {
                            truncated = true;
                            break 'outer;
                        }
                    }
                }
            }
        }

        Ok(json!({ "query": query, "matches": matches, "truncated": truncated }))
    }
}
