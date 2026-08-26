# Session Handler Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist the transcript to disk as JSONL during a run and resume it later (`--continue`, `--resume <id>`, `/sessions` in the TUI), per `docs/superpowers/specs/2026-08-21-session-handler-design.md`.

**Architecture:** A `SessionHandler` extension in `crates/extensions` appends `Context` messages to an append-only JSONL file keyed by workspace; a loader rebuilds a `Context` for resume. The CLI seeds the worker's context from a loaded session and registers the handler; `/sessions` lists and switches sessions through the worker. No kernel changes.

**Tech Stack:** Rust, serde/serde_json (already workspace deps), std::fs. No new dependencies.

**Spec deviation (fold back into spec in Task 3):** the spec says the extension subscribes to `after_model`, but `agent_loop.rs:62` runs `after_model` BEFORE the assistant message is pushed (`:66`/`:70`), so there is never anything new to record there. Subscriptions are `before_model` + `on_agent_end` only.

---

### Task 1: Session file format — meta, ids, workspace key, load/list

**Files:**
- Create: `crates/extensions/src/session.rs`
- Modify: `crates/extensions/src/lib.rs`
- Test: inline `#[cfg(test)] mod tests` in `session.rs`

- [ ] **Step 1: Create the module skeleton with data types and helpers**

Create `crates/extensions/src/session.rs`:

```rust
//! Session persistence: the transcript as append-only JSONL, one file per
//! session, so a run survives its process and can be resumed later.
//!
//! Line 1 is a [`SessionMeta`] header; every following line is one
//! serialized [`Message`]. A crash loses at most the line being written,
//! and the loader knows how to drop a truncated final line.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use orca_harness_core::{Context, Extension, ExtensionError, Message, Subscriptions};

pub const SESSION_FORMAT_VERSION: u32 = 1;

/// The header record on line 1 of every session file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub v: u32,
    pub id: String,
    /// Unix seconds.
    pub created_at: u64,
    pub workspace: String,
    pub model: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("session file {path:?}: missing header line")]
    MissingHeader { path: PathBuf },
    #[error("session file {path:?}: line {line}: {source}")]
    Corrupt {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("session file {path:?}: unsupported version {found} (this build reads v{SESSION_FORMAT_VERSION})")]
    UnsupportedVersion { path: PathBuf, found: u32 },
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

static SESSION_SEQ: AtomicU32 = AtomicU32::new(0);

/// Timestamp-prefixed (zero-padded, so lexical order is creation order)
/// with pid and a process-local sequence to break same-second ties.
pub fn new_session_id() -> String {
    let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{:010}-{:05x}-{seq}", unix_now(), std::process::id() & 0xf_ffff)
}

/// A filesystem-safe directory name for a workspace path: a readable
/// tail plus an FNV-1a hash so distinct paths never collide. FNV is
/// implemented inline so the key is stable across Rust versions
/// (std's DefaultHasher makes no such promise).
pub fn workspace_key(workspace: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in workspace.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mapped: String = workspace
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let tail = mapped.trim_matches('-');
    // Mapped chars are all single-byte, so byte slicing is safe.
    let tail = if tail.is_empty() {
        "ws"
    } else {
        &tail[tail.len().saturating_sub(24)..]
    };
    format!("{tail}-{hash:016x}")
}
```

- [ ] **Step 2: Write failing tests for load/list**

Append to `session.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_core::{ToolCall, ToolResult};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "orca-session-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn meta(id: &str) -> SessionMeta {
        SessionMeta {
            v: SESSION_FORMAT_VERSION,
            id: id.into(),
            created_at: 1_755_000_000,
            workspace: "/tmp/ws".into(),
            model: "test-model".into(),
        }
    }

    fn write_session(dir: &Path, meta: &SessionMeta, lines: &[String], terminated: bool) -> PathBuf {
        let path = dir.join(format!("{}.jsonl", meta.id));
        let mut body = serde_json::to_string(meta).unwrap() + "\n";
        for (index, line) in lines.iter().enumerate() {
            body.push_str(line);
            if terminated || index + 1 < lines.len() {
                body.push('\n');
            }
        }
        fs::write(&path, body).unwrap();
        path
    }

    fn msg_lines(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .map(|m| serde_json::to_string(m).unwrap())
            .collect()
    }

    #[test]
    fn load_round_trips_header_and_messages() {
        let dir = temp_dir("roundtrip");
        let messages = vec![
            Message::System { content: "sys".into() },
            Message::User { content: "hi".into() },
            Message::Assistant { content: Some("yo".into()), tool_calls: vec![] },
        ];
        let path = write_session(&dir, &meta("0000000001-a-0"), &msg_lines(&messages), true);
        let loaded = SessionFile::load(&path).unwrap();
        assert_eq!(loaded.meta, meta("0000000001-a-0"));
        assert!(loaded.warnings.is_empty());
        assert_eq!(
            serde_json::to_string(loaded.context.messages()).unwrap(),
            serde_json::to_string(&messages).unwrap(),
        );
    }

    #[test]
    fn truncated_final_line_is_dropped_with_warning() {
        let dir = temp_dir("truncated");
        let messages = vec![Message::User { content: "hi".into() }];
        let mut lines = msg_lines(&messages);
        lines.push(r#"{"User":{"conte"#.into());
        let path = write_session(&dir, &meta("0000000001-a-0"), &lines, false);
        let loaded = SessionFile::load(&path).unwrap();
        assert_eq!(loaded.warnings.len(), 1);
        assert_eq!(loaded.context.messages().len(), 1);
    }

    #[test]
    fn corrupt_middle_line_is_an_error() {
        let dir = temp_dir("corrupt");
        let mut lines = vec!["not json".to_string()];
        lines.extend(msg_lines(&[Message::User { content: "hi".into() }]));
        let path = write_session(&dir, &meta("0000000001-a-0"), &lines, true);
        match SessionFile::load(&path) {
            Err(SessionError::Corrupt { line, .. }) => assert_eq!(line, 2),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_version_is_an_error() {
        let dir = temp_dir("version");
        let mut m = meta("0000000001-a-0");
        m.v = 99;
        let path = write_session(&dir, &m, &[], true);
        assert!(matches!(
            SessionFile::load(&path),
            Err(SessionError::UnsupportedVersion { found: 99, .. })
        ));
    }

    #[test]
    fn dangling_trailing_assistant_tool_calls_are_dropped() {
        let dir = temp_dir("dangling");
        let messages = vec![
            Message::User { content: "hi".into() },
            Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                }],
            },
        ];
        let path = write_session(&dir, &meta("0000000001-a-0"), &msg_lines(&messages), true);
        let loaded = SessionFile::load(&path).unwrap();
        assert_eq!(loaded.warnings.len(), 1);
        assert_eq!(loaded.context.messages().len(), 1);
    }

    #[test]
    fn assistant_tool_calls_with_results_survive_load() {
        let dir = temp_dir("paired");
        let call = ToolCall { id: "c1".into(), name: "shell".into(), arguments: serde_json::json!({}) };
        let messages = vec![
            Message::Assistant { content: None, tool_calls: vec![call.clone()] },
            Message::Tool { results: vec![ToolResult::ok(&call, serde_json::json!("out"))] },
        ];
        let path = write_session(&dir, &meta("0000000001-a-0"), &msg_lines(&messages), true);
        let loaded = SessionFile::load(&path).unwrap();
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.context.messages().len(), 2);
    }

    #[test]
    fn list_is_newest_first_and_skips_non_sessions() {
        let dir = temp_dir("list");
        write_session(&dir, &meta("0000000001-a-0"), &[], true);
        write_session(&dir, &meta("0000000002-a-0"), &[], true);
        fs::write(dir.join("junk.txt"), "junk").unwrap();
        fs::write(dir.join("bad.jsonl"), "not a header\n").unwrap();
        let sessions = SessionFile::list(&dir);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].meta.id, "0000000002-a-0");
        assert_eq!(sessions[1].meta.id, "0000000001-a-0");
    }

    #[test]
    fn workspace_key_is_stable_and_filesystem_safe() {
        let a = workspace_key("/Users/dev/my project");
        assert_eq!(a, workspace_key("/Users/dev/my project"));
        assert_ne!(a, workspace_key("/Users/dev/my-project"));
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        assert!(workspace_key("///").starts_with("ws-"));
    }

    #[test]
    fn session_ids_sort_chronologically_and_never_collide() {
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a, b);
        assert!(a < b || a.split('-').next() == b.split('-').next());
    }
}
```

Wire the module so tests compile: in `crates/extensions/src/lib.rs` add `mod session;` after `mod retry;` and a temporary `pub use session::{new_session_id, workspace_key, SessionError, SessionFile, SessionMeta};` (the full export list lands in Task 3).

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p orca-harness-extensions session 2>&1 | tail -20`
Expected: compile error — `SessionFile` and `LoadedSession` not yet defined.

- [ ] **Step 4: Implement `SessionFile` (list/load)**

Append to `session.rs` (above the tests module):

```rust
/// One recorded session on disk, as returned by [`SessionFile::list`].
#[derive(Debug, Clone)]
pub struct SessionFile {
    pub path: PathBuf,
    pub meta: SessionMeta,
}

/// A session parsed back off disk.
pub struct LoadedSession {
    pub meta: SessionMeta,
    pub context: Context,
    /// Non-fatal repairs made while loading (dropped crash artifacts).
    pub warnings: Vec<String>,
}

impl SessionFile {
    /// Sessions under `dir`, newest first (ids are timestamp-prefixed).
    /// Files whose header does not parse are skipped: list feeds
    /// pickers; load is where corruption is reported.
    pub fn list(dir: &Path) -> Vec<SessionFile> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut sessions: Vec<SessionFile> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "jsonl") {
                    return None;
                }
                let meta = read_header(&path)?;
                Some(SessionFile { path, meta })
            })
            .collect();
        sessions.sort_by(|a, b| b.meta.id.cmp(&a.meta.id));
        sessions
    }

    pub fn load(path: &Path) -> Result<LoadedSession, SessionError> {
        let text = fs::read_to_string(path)?;
        let complete = text.ends_with('\n');
        let lines: Vec<&str> = text.lines().collect();
        let Some(header) = lines.first().filter(|l| !l.trim().is_empty()) else {
            return Err(SessionError::MissingHeader { path: path.to_path_buf() });
        };
        let meta: SessionMeta =
            serde_json::from_str(header).map_err(|source| SessionError::Corrupt {
                path: path.to_path_buf(),
                line: 1,
                source,
            })?;
        if meta.v != SESSION_FORMAT_VERSION {
            return Err(SessionError::UnsupportedVersion {
                path: path.to_path_buf(),
                found: meta.v,
            });
        }
        let mut warnings = Vec::new();
        let mut messages: Vec<Message> = Vec::new();
        for (index, line) in lines.iter().enumerate().skip(1) {
            match serde_json::from_str::<Message>(line) {
                Ok(message) => messages.push(message),
                // An unterminated final line is the crash artifact the
                // format anticipates; anything else is corruption.
                Err(_) if index + 1 == lines.len() && !complete => {
                    warnings.push(format!(
                        "dropped truncated final line {} of {}",
                        index + 1,
                        path.display()
                    ));
                }
                Err(source) => {
                    return Err(SessionError::Corrupt {
                        path: path.to_path_buf(),
                        line: index + 1,
                        source,
                    })
                }
            }
        }
        // A trailing assistant message with unanswered tool calls cannot
        // be replayed to a chat-completions endpoint; drop it.
        if let Some(Message::Assistant { tool_calls, .. }) = messages.last() {
            if !tool_calls.is_empty() {
                warnings.push(format!(
                    "dropped trailing assistant tool calls with no results in {}",
                    path.display()
                ));
                messages.pop();
            }
        }
        let mut context = Context::new();
        for message in messages {
            context.push(message);
        }
        Ok(LoadedSession {
            meta,
            context,
            warnings,
        })
    }
}

fn read_header(path: &Path) -> Option<SessionMeta> {
    let file = File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    serde_json::from_str(&first).ok()
}
```

Note: `is_none_or` needs Rust 1.82+; if the build errors on it, use `!path.extension().map_or(false, |ext| ext == "jsonl")` — mind the inversion (return None when the extension is missing OR not `jsonl`).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p orca-harness-extensions session 2>&1 | tail -20`
Expected: all Task 1 tests PASS (unused-import warnings for `Extension`/`async_trait` etc. are fine until Task 2; silence temporarily with `#[allow(unused_imports)]` on the import block if clippy is run, or just leave until Task 2).

- [ ] **Step 6: Commit**

```bash
git add crates/extensions/src/session.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): session file format — JSONL header, load, list"
```

---

### Task 2: SessionHandler — recorder and Extension impl

**Files:**
- Modify: `crates/extensions/src/session.rs`
- Test: inline tests in `session.rs`

- [ ] **Step 1: Write failing tests**

Append inside `mod tests`:

```rust
    #[test]
    fn sync_appends_only_new_messages() {
        let dir = temp_dir("sync");
        let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
        let mut context = Context::new();
        context.push_system("sys");
        context.push_user("hi");
        handler.sync(&context);
        context.push_assistant_text("yo");
        handler.sync(&context);
        handler.sync(&context); // no-op

        let loaded = SessionFile::load(&handler.path()).unwrap();
        assert_eq!(
            serde_json::to_string(loaded.context.messages()).unwrap(),
            serde_json::to_string(context.messages()).unwrap(),
        );
    }

    #[test]
    fn sync_rewrites_after_context_shrinks() {
        let dir = temp_dir("shrink");
        let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
        let mut context = Context::new();
        context.push_system("sys");
        context.push_user("one");
        context.push_user("two");
        handler.sync(&context);

        // Compaction replaced the transcript with a shorter one.
        let mut compacted = Context::new();
        compacted.push_system("sys");
        compacted.push_user("summary");
        handler.sync(&compacted);
        compacted.push_user("after");
        handler.sync(&compacted);

        let loaded = SessionFile::load(&handler.path()).unwrap();
        assert_eq!(
            serde_json::to_string(loaded.context.messages()).unwrap(),
            serde_json::to_string(compacted.messages()).unwrap(),
        );
    }

    #[test]
    fn resume_continues_appending_to_the_same_file() {
        let dir = temp_dir("resume");
        let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
        let path = handler.path();
        let mut context = Context::new();
        context.push_system("sys");
        handler.sync(&context);
        drop(handler);

        let (handler, loaded) = SessionHandler::resume(&path).unwrap();
        assert_eq!(loaded.context.messages().len(), 1);
        let mut context = loaded.context;
        context.push_user("again");
        handler.sync(&context);

        let reloaded = SessionFile::load(&path).unwrap();
        assert_eq!(reloaded.context.messages().len(), 2);
    }

    #[test]
    fn start_new_rotates_to_a_fresh_file() {
        let dir = temp_dir("rotate");
        let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
        let first = handler.path();
        let mut context = Context::new();
        context.push_system("sys");
        handler.sync(&context);

        let new_id = handler.start_new().unwrap();
        assert_ne!(handler.path(), first);
        assert_eq!(handler.session_id(), new_id);
        let mut fresh = Context::new();
        fresh.push_system("sys2");
        handler.sync(&fresh);

        assert_eq!(SessionFile::load(&first).unwrap().context.messages().len(), 1);
        let second = SessionFile::load(&handler.path()).unwrap();
        assert_eq!(second.context.messages().len(), 1);
        assert!(SessionFile::list(&dir).len() == 2);
    }

    #[test]
    fn switch_to_adopts_another_session() {
        let dir = temp_dir("switch");
        let first = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
        let mut context = Context::new();
        context.push_system("old sys");
        context.push_user("old");
        first.sync(&context);
        let old_path = first.path();
        drop(first);

        let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap();
        let loaded = handler.switch_to(&old_path).unwrap();
        assert_eq!(loaded.context.messages().len(), 2);
        assert_eq!(handler.path(), old_path);
        let mut context = loaded.context;
        context.push_user("new");
        handler.sync(&context);
        assert_eq!(SessionFile::load(&old_path).unwrap().context.messages().len(), 3);
    }

    #[test]
    fn write_error_disables_recording_and_warns_once() {
        use std::sync::atomic::AtomicUsize;
        let dir = temp_dir("disable");
        let warned = std::sync::Arc::new(AtomicUsize::new(0));
        let count = warned.clone();
        let handler = SessionHandler::create(&dir, "/tmp/ws", "test-model")
            .unwrap()
            .on_warn(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
            });
        // Swap the live handle for a read-only one so appends fail.
        handler.inner.lock().unwrap().file = File::open(handler.path()).unwrap();

        let mut context = Context::new();
        context.push_system("sys");
        handler.sync(&context);
        context.push_user("more");
        handler.sync(&context);

        assert_eq!(warned.load(Ordering::SeqCst), 1);
        let loaded = SessionFile::load(&handler.path()).unwrap();
        assert_eq!(loaded.context.messages().len(), 0);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p orca-harness-extensions session 2>&1 | tail -5`
Expected: compile error — `SessionHandler` not defined.

- [ ] **Step 3: Implement `SessionHandler`**

Append to `session.rs` (above `mod tests`):

```rust
/// Where a live session is being written.
struct Recorder {
    dir: PathBuf,
    meta: SessionMeta,
    path: PathBuf,
    file: File,
    /// Messages already persisted.
    cursor: usize,
    /// Set on the first write error; every later sync is a no-op.
    disabled: bool,
}

/// Records the transcript as the run progresses. Registered as an
/// Extension; hosts also drive it directly (`sync` after post-run
/// repairs, `start_new_with_context` for /clear, `start_new` for hosts
/// that will sync later, and `switch_to` for /sessions).
pub struct SessionHandler {
    inner: Mutex<Recorder>,
    warn: Box<dyn Fn(&str) + Send + Sync>,
}

impl SessionHandler {
    pub fn create(
        dir: impl Into<PathBuf>,
        workspace: &str,
        model: &str,
    ) -> std::io::Result<Self> {
        let dir = dir.into();
        let meta = SessionMeta {
            v: SESSION_FORMAT_VERSION,
            id: new_session_id(),
            created_at: unix_now(),
            workspace: workspace.to_string(),
            model: model.to_string(),
        };
        let (path, file) = open_new(&dir, &meta)?;
        Ok(Self {
            inner: Mutex::new(Recorder {
                dir,
                meta,
                path,
                file,
                cursor: 0,
                disabled: false,
            }),
            warn: Box::new(|message| eprintln!("{message}")),
        })
    }

    /// Route warnings somewhere visible (the CLI sends a TUI notice).
    /// Persistence failures never fail the run; they surface here.
    pub fn on_warn(mut self, warn: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.warn = Box::new(warn);
        self
    }

    /// Reopen a recorded session for appending. If the loader had to
    /// drop crash artifacts, the file is rewritten to match the loaded
    /// context first, so file and cursor never disagree.
    pub fn resume(path: &Path) -> Result<(Self, LoadedSession), SessionError> {
        let loaded = SessionFile::load(path)?;
        let file = reopen(path, &loaded)?;
        let handler = Self {
            inner: Mutex::new(Recorder {
                dir: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
                meta: loaded.meta.clone(),
                path: path.to_path_buf(),
                file,
                cursor: loaded.context.messages().len(),
                disabled: false,
            }),
            warn: Box::new(|message| eprintln!("{message}")),
        };
        Ok((handler, loaded))
    }

    /// Create and fully persist a fresh session before adopting it. Used by
    /// /clear so failure leaves the old recorder active.
    pub fn start_new_with_context(&self, context: &Context) -> std::io::Result<String> {
        let mut rec = self.inner.lock().unwrap();
        let meta = SessionMeta {
            v: SESSION_FORMAT_VERSION,
            id: new_session_id(),
            created_at: unix_now(),
            workspace: rec.meta.workspace.clone(),
            model: rec.meta.model.clone(),
        };
        let (path, mut file) = open_new(&rec.dir, &meta)?;
        if let Err(err) = append(&mut file, context.messages()) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(err);
        }
        rec.meta = meta;
        rec.path = path;
        rec.file = file;
        rec.cursor = context.messages().len();
        rec.disabled = false;
        Ok(rec.meta.id.clone())
    }

    /// Rotate to an empty fresh session for hosts that will sync later.
    pub fn start_new(&self) -> std::io::Result<String> {
        let mut rec = self.inner.lock().unwrap();
        let meta = SessionMeta {
            v: SESSION_FORMAT_VERSION,
            id: new_session_id(),
            created_at: unix_now(),
            workspace: rec.meta.workspace.clone(),
            model: rec.meta.model.clone(),
        };
        let (path, file) = open_new(&rec.dir, &meta)?;
        rec.meta = meta;
        rec.path = path;
        rec.file = file;
        rec.cursor = 0;
        rec.disabled = false;
        Ok(rec.meta.id.clone())
    }

    /// Switch recording to a previously recorded session (used by
    /// /sessions). Returns the loaded transcript for the host to adopt.
    pub fn switch_to(&self, path: &Path) -> Result<LoadedSession, SessionError> {
        let loaded = SessionFile::load(path)?;
        let file = reopen(path, &loaded)?;
        let mut rec = self.inner.lock().unwrap();
        rec.meta = loaded.meta.clone();
        rec.path = path.to_path_buf();
        rec.file = file;
        rec.cursor = loaded.context.messages().len();
        rec.disabled = false;
        Ok(loaded)
    }

    pub fn session_id(&self) -> String {
        self.inner.lock().unwrap().meta.id.clone()
    }

    pub fn path(&self) -> PathBuf {
        self.inner.lock().unwrap().path.clone()
    }

    /// Bring the file up to date with the context. Append-only in the
    /// common case; a context shorter than what was persisted means it
    /// was rewritten (compaction or host-driven reset recovery), so rewrite the file.
    pub fn sync(&self, context: &Context) {
        let mut rec = self.inner.lock().unwrap();
        if rec.disabled {
            return;
        }
        let messages = context.messages();
        let result = if messages.len() < rec.cursor {
            match rewrite(&rec.path, &rec.meta, messages) {
                Ok(file) => {
                    rec.file = file;
                    Ok(())
                }
                Err(err) => Err(err),
            }
        } else {
            append(&mut rec.file, &messages[rec.cursor..])
        };
        match result {
            Ok(()) => rec.cursor = messages.len(),
            Err(err) => {
                rec.disabled = true;
                (self.warn)(&format!(
                    "session recording disabled: {err} ({})",
                    rec.path.display()
                ));
            }
        }
    }
}

#[async_trait]
impl Extension for SessionHandler {
    fn name(&self) -> &str {
        "session"
    }

    // after_model is deliberately absent: the kernel runs it before the
    // assistant message is pushed, so there is never anything new to
    // record there. before_model catches everything between model calls;
    // on_agent_end catches the final assistant message.
    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_model().on_agent_end()
    }

    async fn before_model(&self, context: &mut Context) -> Result<(), ExtensionError> {
        self.sync(context);
        Ok(())
    }

    async fn on_agent_end(&self, context: &Context) {
        self.sync(context);
    }
}

fn open_new(dir: &Path, meta: &SessionMeta) -> std::io::Result<(PathBuf, File)> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.jsonl", meta.id));
    let mut file = OpenOptions::new().create_new(true).append(true).open(&path)?;
    if let Err(err) = write_header(&mut file, meta) {
        drop(file);
        let _ = fs::remove_file(&path);
        return Err(err);
    }
    Ok((path, file))
}

fn reopen(path: &Path, loaded: &LoadedSession) -> Result<File, SessionError> {
    if loaded.warnings.is_empty() {
        Ok(OpenOptions::new().append(true).open(path)?)
    } else {
        Ok(rewrite(path, &loaded.meta, loaded.context.messages())?)
    }
}

fn write_header(file: &mut File, meta: &SessionMeta) -> std::io::Result<()> {
    let header = serde_json::to_string(meta).map_err(std::io::Error::other)?;
    writeln!(file, "{header}")
}

fn append(file: &mut File, messages: &[Message]) -> std::io::Result<()> {
    for message in messages {
        let line = serde_json::to_string(message).map_err(std::io::Error::other)?;
        writeln!(file, "{line}")?;
    }
    file.flush()
}

/// Truncate and rewrite; the returned handle is positioned at the end,
/// so subsequent appends through it continue correctly.
fn rewrite(path: &Path, meta: &SessionMeta, messages: &[Message]) -> std::io::Result<File> {
    let mut file = File::create(path)?;
    write_header(&mut file, meta)?;
    append(&mut file, messages)?;
    Ok(file)
}
```

Note for the `write_error_disables_recording_and_warns_once` test: it reaches into `handler.inner` — that field is private, which is exactly why the test lives in the inline `mod tests` (same module). Do not make `inner` public for it.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p orca-harness-extensions session 2>&1 | tail -20`
Expected: all session tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/session.rs
git commit -m "feat(extensions): SessionHandler records and resumes transcripts"
```

---

### Task 3: Exports, integration test, spec correction

**Files:**
- Modify: `crates/extensions/src/lib.rs`
- Modify: `docs/superpowers/specs/2026-08-21-session-handler-design.md`
- Test: Create `crates/extensions/tests/session.rs`

- [ ] **Step 1: Finalize exports and crate docs**

In `crates/extensions/src/lib.rs`, make the export line:

```rust
pub use session::{
    new_session_id, workspace_key, LoadedSession, SessionError, SessionFile, SessionHandler,
    SessionMeta, SESSION_FORMAT_VERSION,
};
```

and add to the crate-level doc list (after the `UsageMeter` bullet):

```rust
//! - [`SessionHandler`] — record the transcript to an append-only JSONL
//!   file and resume it later; durable memory as an Extension concern.
```

- [ ] **Step 2: Write the record-and-resume integration test**

Create `crates/extensions/tests/session.rs`:

```rust
//! Record a scripted run, resume it, and continue: the loaded context
//! must equal what the extension saw, and appends must keep working.

use std::sync::Arc;

use async_trait::async_trait;
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    Agent, CancellationToken, Context, ModelResponse, Tool, ToolContext, ToolError, ToolSchema,
};
use orca_harness_extensions::{SessionFile, SessionHandler};

struct Echo;

#[async_trait]
impl Tool for Echo {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "echo".into(),
            description: "echo the input".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<serde_json::Value, ToolError> {
        Ok(input)
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("orca-session-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test]
async fn records_a_tool_round_and_resumes_it() {
    let dir = temp_dir("roundtrip");

    // Run 1: a tool round, recorded.
    let model = ScriptedModel::tool_round(
        vec![call("c1", "echo", serde_json::json!({"x": 1}))],
        "done",
    );
    let handler = Arc::new(SessionHandler::create(&dir, "/tmp/ws", "test-model").unwrap());
    let agent = Agent::new(model).extension_arc(handler.clone()).tool(Echo);
    let mut context = Context::new();
    context.push_system("sys");
    context.push_user("go");
    agent
        .run_context(&mut context, CancellationToken::new())
        .await
        .unwrap();

    let sessions = SessionFile::list(&dir);
    assert_eq!(sessions.len(), 1);
    let loaded = SessionFile::load(&sessions[0].path).unwrap();
    assert!(loaded.warnings.is_empty(), "warnings: {:?}", loaded.warnings);
    assert_eq!(
        serde_json::to_string(loaded.context.messages()).unwrap(),
        serde_json::to_string(context.messages()).unwrap(),
    );

    // Run 2: resume the same file and continue the conversation.
    let (handler, loaded) = SessionHandler::resume(&sessions[0].path).unwrap();
    let handler = Arc::new(handler);
    let model = ScriptedModel::new(vec![ModelResponse::final_text("again".into())]);
    let agent = Agent::new(model).extension_arc(handler.clone());
    let mut context = loaded.context;
    context.push_user("more");
    agent
        .run_context(&mut context, CancellationToken::new())
        .await
        .unwrap();

    let reloaded = SessionFile::load(&sessions[0].path).unwrap();
    assert_eq!(
        serde_json::to_string(reloaded.context.messages()).unwrap(),
        serde_json::to_string(context.messages()).unwrap(),
    );
    let _ = std::fs::remove_dir_all(&dir);
}
```

If `ModelResponse::final_text` takes `&str` or its variant shape differs, check `crates/harness-core/src/model.rs` and adjust the call — `testing.rs:58` uses `ModelResponse::final_text(final_answer.into())` so a `String` argument is right.

- [ ] **Step 3: Run the integration test**

Run: `cargo test -p orca-harness-extensions --test session 2>&1 | tail -10`
Expected: PASS. The final `"done"`/`"again"` assistant text being present in the file proves `on_agent_end` covers the last message.

- [ ] **Step 4: Correct the spec's subscription claim**

In `docs/superpowers/specs/2026-08-21-session-handler-design.md`, replace the paragraph starting `Subscriptions: before_model, after_model, on_agent_end only.` — change the subscription list to `before_model` and `on_agent_end`, and replace the sentence "`after_model` catches assistant turns" with: "`after_model` is not used: the kernel runs it before the assistant message is pushed to the context (`agent_loop.rs`), so there is never anything new to record there; the next `before_model` or the final `on_agent_end` catches assistant turns."

- [ ] **Step 5: Run the whole extensions crate test suite**

Run: `cargo test -p orca-harness-extensions 2>&1 | tail -5`
Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/extensions docs/superpowers/specs/2026-08-21-session-handler-design.md
git commit -m "feat(extensions): export session API, record/resume integration test"
```

---

### Task 4: CLI flags, sessions dir, and open_session

**Files:**
- Modify: `crates/cli/src/config.rs`
- Modify: `crates/cli/src/main.rs` (USAGE, `Config`, `parse_args`, new `open_session`)

- [ ] **Step 1: Add the sessions dir helper**

In `crates/cli/src/config.rs`, after `config_path()`:

```rust
/// The sessions directory, or `None` when no home directory is
/// resolvable. Sessions live next to the config file.
pub fn sessions_dir() -> Option<PathBuf> {
    Some(config_path()?.parent()?.join("sessions"))
}
```

- [ ] **Step 2: Add flags to Config, parse_args, and USAGE**

In `crates/cli/src/main.rs`:

Add to the `Config` struct after `subagent_depth`:

```rust
    pub continue_latest: bool,
    pub resume_id: Option<String>,
    pub no_session: bool,
```

In `parse_args`, add locals after `subagent_depth`:

```rust
    let mut continue_latest = false;
    let mut resume_id: Option<String> = None;
    let mut no_session = false;
```

match arms after `"--theme"`:

```rust
            "--continue" => continue_latest = true,
            "--resume" => resume_id = Some(value("--resume")?),
            "--no-session" => no_session = true,
```

and the three fields in the final `Ok(Config { ... })` after `subagent_depth,`.

Add to `USAGE` after the `--theme` line:

```text
  --continue         resume the latest recorded session for this workspace
  --resume ID        resume a recorded session by id (a unique prefix works)
  --no-session       do not record this session to disk
```

and extend the trailing prose paragraph with: `Sessions are recorded under ~/.config/orcacode/sessions/ per workspace; /sessions in the TUI lists and resumes them.`

- [ ] **Step 3: Add open_session**

In `main.rs`, imports: extend the `orca_harness_extensions` use to include `workspace_key, SessionFile, SessionHandler`.

Add above `run_mode`:

```rust
/// Create a fresh session file, or resume one when --continue/--resume
/// asked. Errors are fatal at startup: recording (or the requested
/// resume) cannot happen, and silently running without it would lose
/// the transcript the user asked to keep.
fn open_session(cfg: &Config, ws: &Workspace) -> Result<(SessionHandler, Option<Context>), String> {
    let base = config::sessions_dir().ok_or("no home directory for session storage")?;
    let scope = workspace_scope(ws);
    let dir = base.join(workspace_key(&scope));
    if cfg.continue_latest || cfg.resume_id.is_some() {
        let sessions = SessionFile::list(&dir);
        let picked = match &cfg.resume_id {
            Some(id) => sessions.into_iter().find(|s| s.meta.id.starts_with(id.as_str())),
            None => sessions.into_iter().next(),
        };
        let picked =
            picked.ok_or_else(|| format!("no session to resume under {}", dir.display()))?;
        let (handler, loaded) = SessionHandler::resume(&picked.path).map_err(|e| e.to_string())?;
        for warning in &loaded.warnings {
            eprintln!("warning: {warning}");
        }
        Ok((handler, Some(loaded.context)))
    } else {
        let handler =
            SessionHandler::create(&dir, &scope, &cfg.model).map_err(|e| e.to_string())?;
        Ok((handler, None))
    }
}
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p orcacode 2>&1 | tail -5`
Expected: success (an unused-function warning for `open_session` is fine until Task 5).

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/config.rs crates/cli/src/main.rs
git commit -m "feat(cli): session flags, sessions dir, open_session"
```

---

### Task 5: Wire recording and resume through worker and headless

**Files:**
- Modify: `crates/cli/src/msg.rs`
- Modify: `crates/cli/src/main.rs` (`run_mode`, `worker`, `build_agent`)
- Modify: `crates/cli/src/headless.rs`
- Modify: `crates/cli/src/tui.rs` (TuiConfig field + minimal UiMsg arms so it compiles; full /sessions UI is Task 6)

- [ ] **Step 1: Extend the channel types**

In `crates/cli/src/msg.rs`, add to `UiMsg` (after `Compacted`):

```rust
    /// A one-line status notice for the transcript (session warnings,
    /// load failures).
    Notice(String),
    /// The worker adopted a previously recorded session.
    SessionLoaded { id: String, messages: usize },
```

Add to `WorkerCmd` (after `Compact`):

```rust
    /// Adopt a recorded session: replace the context and record there.
    LoadSession { path: std::path::PathBuf },
```

- [ ] **Step 2: Rework run_mode**

In `main.rs` `run_mode`, after `let stats = BackgroundStats::new();` insert:

```rust
    let session = if cfg.no_session {
        None
    } else {
        match open_session(&cfg, &ws) {
            Ok(opened) => Some(opened),
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        }
    };
```

Replace the headless branch with:

```rust
    if cfg.prompt.is_some() {
        let (handler, resumed) = match session {
            Some((handler, resumed)) => (Some(Arc::new(handler)), resumed),
            None => (None, None),
        };
        let code = headless::run(&cfg, endpoint.build_model(), &ws, &system, handler, resumed).await;
        return ExitCode::from(code as u8);
    }
```

After the two `mpsc::unbounded_channel()` lines, add:

```rust
    // Session warnings surface as transcript notices; recording failures
    // must be visible but never fatal mid-run.
    let (session, resumed) = match session {
        Some((handler, resumed)) => {
            let ui = ui_tx.clone();
            let handler = handler.on_warn(move |message| {
                let _ = ui.send(UiMsg::Notice(message.to_string()));
            });
            (Some(Arc::new(handler)), resumed)
        }
        None => (None, None),
    };
    let mut context = match resumed {
        // A recorded transcript already begins with its system prompt.
        Some(context) => {
            let _ = ui_tx.send(UiMsg::Notice(format!(
                "resumed session {} ({} messages)",
                session.as_ref().map(|s| s.session_id()).unwrap_or_default(),
                context.messages().len()
            )));
            context
        }
        None => {
            let mut context = Context::new();
            context.push_system(&system);
            context
        }
    };
```

(`let mut context` — the worker takes ownership; `mut` silences nothing here, drop the `mut` if the compiler warns.)

Thread the handler into the build closure: inside the `let build = { ... }` block add `let session = session.clone();` beside the other clones, and pass `&session` as a new final argument to `build_agent`. Pass `session.clone(), context` to `worker` (new params, see Step 3), and set the new `TuiConfig` field:

```rust
        session_id: session.as_ref().map(|s| s.session_id()),
```

- [ ] **Step 3: Rework worker**

Change the `worker` signature:

```rust
async fn worker<F>(
    mut agent: Agent<Arc<dyn Model>>,
    system: String,
    mut endpoint: Endpoint,
    build: F,
    store: TruncationStore,
    session: Option<Arc<SessionHandler>>,
    mut context: Context,
    mut commands: mpsc::UnboundedReceiver<WorkerCmd>,
    ui: mpsc::UnboundedSender<UiMsg>,
) where
    F: Fn(&Endpoint) -> Agent<Arc<dyn Model>>,
{
    spawn_window_probe(&endpoint, ui.clone());
```

(delete the old `let mut context = Context::new(); context.push_system(&system);` lines — the context now arrives seeded).

In the `Run` arm, after `repair_dangling_tool_calls(&mut context);`:

```rust
                // The repair lands after on_agent_end fired; catch up so
                // the file never ends in dangling tool calls.
                if let Some(session) = &session {
                    session.sync(&context);
                }
```

In the `Clear` arm, prepare a system-only context and persist it transactionally
before replacing any in-memory state:

```rust
                let mut fresh = Context::new();
                fresh.push_system(&system);
                let new_session_id = match session.as_deref() {
                    Some(session) => match session.start_new_with_context(&fresh) {
                        Ok(id) => Some(id),
                        Err(err) => {
                            let _ = ui.send(UiMsg::Notice(format!(
                                "session not cleared: could not preserve history: {err}"
                            )));
                            continue;
                        }
                    },
                    None => None,
                };
                context = fresh;
                // Clear conversation-owned state and rebuild the agent here.
                // Send SessionCleared only after the old agent has been dropped.
                let _ = ui.send(UiMsg::SessionCleared { id: new_session_id });
```

In the `Compact` arm, after the `compact(...)` call:

```rust
                if let Some(session) = &session {
                    session.sync(&context);
                }
```

New arm after `Compact`:

```rust
            WorkerCmd::LoadSession { path } => {
                let Some(session) = &session else {
                    let _ = ui.send(UiMsg::Notice(
                        "session recording is disabled (--no-session)".into(),
                    ));
                    continue;
                };
                match session.switch_to(&path) {
                    Ok(loaded) => {
                        for warning in &loaded.warnings {
                            let _ = ui.send(UiMsg::Notice(warning.clone()));
                        }
                        context = loaded.context;
                        let _ = ui.send(UiMsg::SessionLoaded {
                            id: loaded.meta.id,
                            messages: context.messages().len(),
                        });
                    }
                    Err(err) => {
                        let _ = ui.send(UiMsg::Notice(format!("session load failed: {err}")));
                    }
                }
            }
```

- [ ] **Step 4: Register the extension in build_agent**

Add a final parameter `session: &Option<Arc<SessionHandler>>` to `build_agent`, and right after the `Agent::new(model)...extension(Approval::new(...))` chain:

```rust
    if let Some(session) = session {
        agent = agent.extension_arc(session.clone());
    }
```

(`Arc<SessionHandler>` unsize-coerces to `Arc<dyn Extension>` at the call.)

- [ ] **Step 5: Wire headless**

In `crates/cli/src/headless.rs`, change the signature and context setup:

```rust
pub async fn run<M: Model + Clone + 'static>(
    cfg: &Config,
    model: M,
    ws: &Workspace,
    system_prompt: &str,
    session: Option<std::sync::Arc<orca_harness_extensions::SessionHandler>>,
    resumed: Option<Context>,
) -> i32 {
```

After the `if !cfg.auto_approve { ... }` block:

```rust
    if let Some(session) = &session {
        agent = agent.extension_arc(session.clone());
    }
```

Replace the `let mut context = Context::new(); context.push_system(system_prompt);` lines:

```rust
    let mut context = match resumed {
        // A recorded transcript already begins with its system prompt.
        Some(context) => context,
        None => {
            let mut context = Context::new();
            context.push_system(system_prompt);
            context
        }
    };
```

(headless keeps the default `eprintln!` warn route.)

- [ ] **Step 6: Keep the TUI compiling**

In `crates/cli/src/tui.rs`:
- Add `pub session_id: Option<String>,` to `TuiConfig` (near `stats`).
- In the `UiMsg` match (near the `UiMsg::Compacted` arm at ~1734), add placeholder-free minimal arms:

```rust
        UiMsg::Notice(text) => {
            app.push_line(Line::from(Span::styled(text, theme().dim)));
        }
        UiMsg::SessionLoaded { id, messages } => {
            reset_conversation_ui(app);
            app.push_line(Line::from(Span::styled(
                format!("resumed session {id} ({messages} messages)"),
                theme().dim,
            )));
        }
```

- Extract the per-conversation reset block into `reset_conversation_ui(app)`.
  The `"clear"` command arm only sends `WorkerCmd::Clear`; call the reset helper
  when `UiMsg::SessionCleared` arrives, after the worker has preserved the old
  transcript and adopted the fresh session. A failed rotation reports a notice
  and leaves the visible conversation unchanged.

```rust
/// Reset every piece of per-conversation UI state. Used by /clear and
/// when the worker adopts another session.
fn reset_conversation_ui(app: &mut App) {
    app.transcript.clear();
    app.pending_history.clear();
    app.scroll = 0;
    app.transcript_max_scroll = 0;
    app.split_inspector_cache = None;
    app.prompt_queue.clear();
    app.tokens_in = 0;
    app.tokens_out = 0;
    app.cache_read_total = 0;
    app.cache_write_total = 0;
    app.usage_steps = 0;
    app.context_tokens = 0;
    app.tool_log.clear();
    app.work_log.clear();
    app.turn_count = 0;
    app.reset_activity();
    app.split_snapshot = None;
}
```

and call it from session-load handling and the worker's successful
`UiMsg::SessionCleared` acknowledgement. Do not reset in the command-dispatch
arm: rotation failure must preserve the visible conversation.

- [ ] **Step 7: Compile and test the CLI crate**

Run: `cargo test -p orcacode 2>&1 | tail -10`
Expected: builds, existing CLI tests PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/cli
git commit -m "feat(cli): record sessions by default, resume via --continue/--resume"
```

---

### Task 6: /sessions command

**Files:**
- Modify: `crates/cli/src/commands.rs`
- Modify: `crates/cli/src/tui.rs`

- [ ] **Step 1: Update the command-registry test first**

In `crates/cli/src/commands.rs` tests, `prefix_matches_rank_before_substring_matches`: append `"sessions"` to the expected vec of the `filter_commands("e")` assertion (it contains an `e`; it will be last because the spec is appended at the registry's end). The `"m"`/`"el"` assertions are unaffected.

Run: `cargo test -p orcacode commands 2>&1 | tail -5`
Expected: FAIL — registry has no `sessions` yet.

- [ ] **Step 2: Register the command**

Append to `COMMANDS` (after the `settings` spec):

```rust
    CommandSpec {
        name: "sessions",
        description: "list recorded sessions for this workspace, or resume one by id",
        category: "Session",
        takes_args: true,
    },
```

Run: `cargo test -p orcacode commands 2>&1 | tail -5`
Expected: PASS.

- [ ] **Step 3: Handle /sessions in the TUI**

In `crates/cli/src/tui.rs`, in the slash-command handler, add before the `if let Some(rest) = command.strip_prefix("theme")` block:

```rust
    if let Some(rest) = command.strip_prefix("sessions") {
        let arg = rest.trim();
        let Some(base) = crate::config::sessions_dir() else {
            app.push_line(Line::from(Span::styled(
                "no home directory for session storage",
                theme().error,
            )));
            return;
        };
        let dir = base.join(orca_harness_extensions::workspace_key(&app.cfg.workspace_root));
        let sessions = orca_harness_extensions::SessionFile::list(&dir);
        if arg.is_empty() {
            if sessions.is_empty() {
                app.push_line(Line::from(Span::styled(
                    "no recorded sessions for this workspace",
                    dim,
                )));
                return;
            }
            for session in sessions.iter().take(20) {
                let current = app.cfg.session_id.as_deref() == Some(session.meta.id.as_str());
                let marker = if current { "  (current)" } else { "" };
                app.push_line(Line::from(Span::styled(
                    format!(
                        "{}  {}  {}{marker}",
                        session.meta.id,
                        age_label(session.meta.created_at),
                        session.meta.model,
                    ),
                    dim,
                )));
            }
            app.push_line(Line::from(Span::styled(
                "/sessions <id> resumes one (a unique prefix works)",
                dim,
            )));
            return;
        }
        match sessions.iter().find(|s| s.meta.id.starts_with(arg)) {
            Some(session) => {
                if worker
                    .send(WorkerCmd::LoadSession {
                        path: session.path.clone(),
                    })
                    .is_err()
                {
                    app.push_line(Line::from(Span::styled(
                        "worker is gone; restart orcacode",
                        theme().error,
                    )));
                } else {
                    app.push_line(Line::from(Span::styled(
                        format!("loading session {}…", session.meta.id),
                        dim,
                    )));
                }
            }
            None => {
                app.push_line(Line::from(Span::styled(
                    format!("no session matching {arg} — /sessions lists them"),
                    theme().error,
                )));
            }
        }
        return;
    }
```

Add the helper near the other free functions in tui.rs:

```rust
/// Compact "how long ago" label for the /sessions listing.
fn age_label(created_at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let delta = now.saturating_sub(created_at);
    match delta {
        0..=59 => format!("{delta}s ago"),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86_399 => format!("{}h ago", delta / 3600),
        _ => format!("{}d ago", delta / 86_400),
    }
}
```

Also, when `SessionLoaded` arrives, keep the current-session marker honest: in that `UiMsg::SessionLoaded` arm (Task 5 Step 6), add `app.cfg.session_id = Some(id.clone());` before pushing the line (adjust the format call to use the clone).

Add to the `/help` listing (the `for entry in [...]` block at ~1646): a line `"/sessions [id] list recorded sessions, or resume one",` next to the other Session entries.

- [ ] **Step 4: Compile and test**

Run: `cargo test -p orcacode 2>&1 | tail -5`
Expected: builds, tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/commands.rs crates/cli/src/tui.rs
git commit -m "feat(cli): /sessions lists and resumes recorded sessions"
```

---

### Task 7: Docs and full verification

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update README**

In the crate layout block, extend the `extensions/` bullet's list with `session recording/resume (JSONL transcripts, --continue / --resume / /sessions)`. In the section describing CLI behavior (near the approvals paragraph around line 86), add one sentence: sessions are recorded per workspace under `~/.config/orcacode/sessions/`; `--continue` resumes the latest, `--resume <id>` a specific one, `/sessions` lists them in the TUI, `--no-session` opts out.

- [ ] **Step 2: Full workspace verification**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets 2>&1 | tail -5 && cargo test --workspace 2>&1 | tail -10`
Expected: fmt clean, no new clippy warnings, all tests PASS.

- [ ] **Step 3: Smoke the flags**

Run: `cargo run -p orcacode -- --help | grep -A2 -- --continue`
Expected: the three new flags print in USAGE.

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: session recording, resume flags, and /sessions"
```

---

## Self-Review Checklist (run after writing, before executing)

- Spec coverage: format+header (T1), extension behavior+cursor+rewrite (T2), resume+list (T1/T2), CLI flags+default recording+/clear rotation (T4/T5), headless (T5), error handling warn-once/disable + corrupt-file behavior (T1/T2), /sessions (T6), out-of-scope items untouched. Spec's `after_model` claim corrected in T3.
- No placeholders: every step carries the code or exact command.
- Type consistency: `SessionHandler::{create,resume,start_new_with_context,start_new,switch_to,sync,session_id,path,on_warn}`, `SessionFile::{list,load}`, `LoadedSession{meta,context,warnings}` used identically across tasks.
