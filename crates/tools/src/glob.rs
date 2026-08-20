//! `glob` — find files by name pattern across the workspace.
//!
//! Pure-std matcher supporting `*` (any run within a segment), `?` (one
//! character), and `**` (any number of path segments), mirroring the
//! literal-`grep` stance: no regex/glob dependency. A pattern without `/`
//! matches file names anywhere in the tree (`*.rs` finds every Rust file);
//! a pattern with `/` is anchored at the search root.

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use orca_harness_core::{Tool, ToolContext, ToolError, ToolSchema};

use crate::workspace::Workspace;

pub struct GlobTool {
    ws: Workspace,
    max_results: usize,
}

impl GlobTool {
    pub fn new(ws: Workspace) -> Self {
        Self {
            ws,
            max_results: 500,
        }
    }
    pub fn max_results(mut self, n: usize) -> Self {
        self.max_results = n;
        self
    }
}

/// Match one pattern segment (no `/`) against one path segment.
/// `*` matches any run of characters, `?` exactly one.
fn match_segment(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // dp[j] = does p[..i] match t[..j]; rolled over i.
    let mut dp = vec![false; t.len() + 1];
    dp[0] = true;
    for i in 1..=p.len() {
        let prev_0 = dp[0];
        dp[0] = prev_0 && p[i - 1] == '*';
        let mut prev = prev_0; // dp[i-1][j-1]
        for j in 1..=t.len() {
            let cur = dp[j]; // dp[i-1][j]
            dp[j] = match p[i - 1] {
                '*' => cur || dp[j - 1],
                '?' => prev,
                c => prev && c == t[j - 1],
            };
            prev = cur;
        }
    }
    dp[t.len()]
}

/// Match pattern segments against path segments, where a `**` segment
/// spans zero or more path segments.
fn match_path(pat: &[&str], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| match_path(rest, &path[skip..])),
        Some((seg, rest)) => match path.split_first() {
            Some((head, tail)) => match_segment(seg, head) && match_path(rest, tail),
            None => false,
        },
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "glob".into(),
            description: "Find files by name pattern. Supports `*` (within a segment), `?`, \
                and `**` (across directories). A pattern without `/` matches file names \
                anywhere in the tree; with `/` it is anchored at the search root."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "e.g. `*.rs`, `src/**/*.test.ts`."},
                    "path": {"type": "string", "default": ".", "description": "Subtree to search."}
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`pattern` (string) is required"))?;
        if pattern.is_empty() {
            return Err(ToolError::msg("`pattern` must not be empty"));
        }
        let start_rel = input.get("path").and_then(Value::as_str).unwrap_or(".");
        let start = self.ws.resolve(start_rel)?;

        // Bare-name patterns search the whole tree.
        let normalized = if pattern.contains('/') {
            pattern.to_string()
        } else {
            format!("**/{pattern}")
        };
        let pat_segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();

        let mut matches = Vec::new();
        let mut truncated = false;
        let mut stack = vec![start.clone()];

        'outer: while let Some(dir) = stack.pop() {
            if ctx.cancellation.is_cancelled() {
                return Err(ToolError::msg("cancelled"));
            }
            let mut rd = match fs::read_dir(&dir).await {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            while let Ok(Some(entry)) = rd.next_entry().await {
                let name = entry.file_name();
                if matches!(
                    name.to_str(),
                    Some(".git") | Some("target") | Some("node_modules")
                ) {
                    continue;
                }
                let path = entry.path();
                let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                if is_dir {
                    stack.push(path);
                    continue;
                }
                // Match against the path relative to the search root.
                let rel = match path.strip_prefix(&start) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let segments: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                let seg_refs: Vec<&str> = segments.iter().map(String::as_str).collect();
                if match_path(&pat_segments, &seg_refs) {
                    matches.push(self.ws.display_rel(&path).to_string_lossy().into_owned());
                    if matches.len() >= self.max_results {
                        truncated = true;
                        break 'outer;
                    }
                }
            }
        }

        matches.sort();
        Ok(json!({ "pattern": pattern, "matches": matches, "truncated": truncated }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_matching() {
        assert!(match_segment("*.rs", "main.rs"));
        assert!(match_segment("*", "anything"));
        assert!(match_segment("?at", "cat"));
        assert!(!match_segment("?at", "flat"));
        assert!(match_segment("a*b*c", "aXbYc"));
        assert!(!match_segment("*.rs", "main.ts"));
        assert!(match_segment("", ""));
        assert!(!match_segment("", "x"));
    }

    #[test]
    fn path_matching() {
        let m = |pat: &str, path: &str| {
            let p: Vec<&str> = pat.split('/').collect();
            let t: Vec<&str> = path.split('/').collect();
            match_path(&p, &t)
        };
        assert!(m("**/*.rs", "src/deep/lib.rs"));
        assert!(m("**/*.rs", "lib.rs"));
        assert!(m("src/**/*.rs", "src/lib.rs"));
        assert!(m("src/**/*.rs", "src/a/b/lib.rs"));
        assert!(!m("src/**/*.rs", "other/lib.rs"));
        assert!(m("src/*/mod.rs", "src/a/mod.rs"));
        assert!(!m("src/*/mod.rs", "src/a/b/mod.rs"));
        assert!(m("**", "any/thing/at/all"));
    }
}
