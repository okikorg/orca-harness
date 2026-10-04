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

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use crate::workspace::Workspace;

/// What a file looked like the last time these tools saw it is
/// [`Stat`](orca_harness_core::Stat) — modified time and length, which
/// together notice an outside edit without hashing contents on every
/// call. A change preserving both is possible in theory and has never
/// been the failure this guards against (an editor saving, a `git
/// checkout`, a build regenerating a file).
///
/// It is the kernel's type rather than a local one so the check means the
/// same thing whether the file lives on this machine or inside a sandbox;
/// the [`Workspace`] reports it from whichever backend it is rooted at.
type Stamp = orca_harness_core::Stat;

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
    pub(crate) async fn restamp(&self, ws: &Workspace, path: &Path) {
        if let Some(stamp) = ws.stat(path).await {
            self.record(path, stamp);
        }
    }

    /// The read-before-write check. `Ok` when the path does not exist
    /// (a create), when it is not a regular file (the write will fail on
    /// its own terms), or when the recorded stamp still matches.
    async fn check_overwrite(
        &self,
        ws: &Workspace,
        path: &Path,
        rel: &str,
    ) -> Result<(), ToolError> {
        let Some(current) = ws.stat_checked(path).await? else {
            return Ok(());
        };
        if current.is_dir {
            return Ok(());
        }
        match self.stamp(path) {
            Some(seen) if seen == current => Ok(()),
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

/// Entries shown when a read misses. Enough to cover a typical source
/// directory without turning the error into a listing.
const NOT_FOUND_HINT_ENTRIES: usize = 40;

/// Occurrences named when an edit is ambiguous. Past this the model is
/// better served by `replaceAll` than by a list.
pub(super) const AMBIGUOUS_LINES_SHOWN: usize = 8;

/// `a, b, c` for short lists, `a, b, c (7 more)` past `cap`. Error hints
/// list things for the model to choose from; the tail count says the
/// list was cut without pretending it is complete.
pub(super) fn capped_list<T: std::fmt::Display>(items: &[T], cap: usize) -> String {
    let shown = items.len().min(cap);
    let mut rendered = items[..shown]
        .iter()
        .map(T::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > shown {
        rendered.push_str(&format!(" ({} more)", items.len() - shown));
    }
    rendered
}

/// The 1-based line of each non-overlapping occurrence of `needle`,
/// rendered for an ambiguity error (`lines 12, 40, 71`). An edit refused
/// for matching more than once should say where, so the retry can pick
/// the right one and add context from around it instead of guessing.
pub(super) fn occurrence_lines(content: &str, needle: &str) -> String {
    // One pass: count newlines only between consecutive matches.
    let mut line = 1;
    let mut scanned = 0;
    let lines: Vec<usize> = content
        .match_indices(needle)
        .map(|(offset, _)| {
            line += content[scanned..offset].matches('\n').count();
            scanned = offset;
            line
        })
        .collect();
    format!("lines {}", capped_list(&lines, AMBIGUOUS_LINES_SHOWN))
}

/// Turn a not-found read into a redirect. A missing path is almost always
/// a guess at a convention the tree does not follow (`tests/mod.rs` where
/// tests are `include!`d), so name the nearest existing ancestor and what
/// it holds: the next call can then be right instead of another guess.
async fn not_found_hint(ws: &Workspace, path: &Path) -> Option<String> {
    let mut dir = path.parent()?;
    loop {
        if ws.stat(dir).await.is_some_and(|stat| stat.is_dir) {
            break;
        }
        if dir == ws.root() {
            return None;
        }
        dir = dir.parent()?;
    }
    let mut names = Vec::new();
    for (name, is_dir) in ws.list(dir).await.ok()? {
        names.push(if is_dir { format!("{name}/") } else { name });
    }
    names.sort();
    let rel = ws.display_rel(dir).to_string_lossy();
    let place = if rel.is_empty() {
        "the workspace root".to_owned()
    } else {
        format!("nearest existing directory `{rel}`")
    };
    if names.is_empty() {
        return Some(format!("; {place} is empty"));
    }
    Some(format!(
        "; {place} contains: {}",
        capped_list(&names, NOT_FOUND_HINT_ENTRIES)
    ))
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
            description: "Read a UTF-8 text file relative to the workspace root. Absolute paths are rejected.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": {
                    "type": "string",
                    "description": "Path relative to the workspace root; absolute paths are rejected."
                } },
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
            Some(_) => self.ws.stat(&path).await,
            None => None,
        };
        let bytes = match self.ws.read(&path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                let mut message = e.to_string();
                // The hint only helps when the path is genuinely absent;
                // a permissions or transport failure is not a wrong guess
                // at the tree's shape.
                if self.ws.stat(&path).await.is_none() {
                    if let Some(hint) = not_found_hint(&self.ws, &path).await {
                        message.push_str(&hint);
                    }
                }
                return Err(ToolError::msg(message));
            }
        };
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
            guard.check_overwrite(&self.ws, &path, &rel).await?;
        }
        self.ws.write(&path, content.as_bytes()).await?;
        if let Some(guard) = &self.guard {
            guard.restamp(&self.ws, &path).await;
        }
        Ok(json!({ "path": rel, "bytesWritten": bytes }))
    }
}

// The test module sits at the end of the file: clippy's
// `items_after_test_module` treats anything below it as misplaced.
#[cfg(test)]
mod tests {
    use super::{capped_list, occurrence_lines, take_string_arg, ReadFileTool};
    use crate::Workspace;
    use orca_harness_core::Tool;
    use serde_json::json;

    #[test]
    fn read_schema_requires_workspace_relative_paths() {
        let schema = ReadFileTool::new(Workspace::new("/work")).schema();
        assert!(schema.description.contains("Absolute paths are rejected."));
        assert_eq!(
            schema.parameters["properties"]["path"]["description"],
            "Path relative to the workspace root; absolute paths are rejected."
        );
    }

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

    #[test]
    fn capped_list_cuts_with_a_count() {
        assert_eq!(capped_list(&[1, 2, 3], 3), "1, 2, 3");
        assert_eq!(capped_list(&[1, 2, 3, 4, 5], 2), "1, 2 (3 more)");
    }

    #[test]
    fn occurrence_lines_are_one_based_and_capped() {
        assert_eq!(occurrence_lines("x\nx\n\nx", "x"), "lines 1, 2, 4");
        assert_eq!(
            occurrence_lines(&"x\n".repeat(10), "x"),
            "lines 1, 2, 3, 4, 5, 6, 7, 8 (2 more)"
        );
    }
}
