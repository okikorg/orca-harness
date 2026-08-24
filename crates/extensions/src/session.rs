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
    /// The session this one was forked from, if any. Optional and
    /// omitted when absent, so files written before forking existed
    /// still load and files written after it are still readable by a
    /// build that predates the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
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
    format!(
        "{:010}-{:05x}-{seq}",
        unix_now(),
        std::process::id() & 0xf_ffff
    )
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
            return Err(SessionError::MissingHeader {
                path: path.to_path_buf(),
            });
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
/// repairs, `reset` for /clear, `switch_to` for /sessions, and
/// `start_new` for hosts that prefer rotation over in-place clears).
pub struct SessionHandler {
    inner: Mutex<Recorder>,
    warn: Box<dyn Fn(&str) + Send + Sync>,
}

impl SessionHandler {
    pub fn create(dir: impl Into<PathBuf>, workspace: &str, model: &str) -> std::io::Result<Self> {
        let dir = dir.into();
        let meta = SessionMeta {
            v: SESSION_FORMAT_VERSION,
            id: new_session_id(),
            created_at: unix_now(),
            workspace: workspace.to_string(),
            model: model.to_string(),
            parent: None,
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

    /// Rotate to a fresh session file (used by /clear). Returns the id.
    pub fn start_new(&self) -> std::io::Result<String> {
        self.rotate(None)
    }

    /// Branch: start a fresh session file that records the current one
    /// as its parent, and leave the current file exactly as it is. The
    /// caller `sync`s the context it wants carried over — the new file's
    /// cursor is zero, so the whole context is written to it.
    pub fn fork(&self) -> std::io::Result<String> {
        let parent = self.session_id();
        self.rotate(Some(parent))
    }

    /// Swap the live file for a new one, optionally recording where it
    /// came from. Recording restarts at zero either way.
    fn rotate(&self, parent: Option<String>) -> std::io::Result<String> {
        let mut rec = self.inner.lock().unwrap();
        let meta = SessionMeta {
            v: SESSION_FORMAT_VERSION,
            id: new_session_id(),
            created_at: unix_now(),
            workspace: rec.meta.workspace.clone(),
            model: rec.meta.model.clone(),
            parent,
        };
        let (path, file) = open_new(&rec.dir, &meta)?;
        rec.meta = meta;
        rec.path = path;
        rec.file = file;
        rec.cursor = 0;
        rec.disabled = false;
        Ok(rec.meta.id.clone())
    }

    /// Empty the current session in place (used by /clear): the file is
    /// truncated back to its header — same id, same path — and
    /// recording restarts from zero.
    pub fn reset(&self) -> std::io::Result<()> {
        let mut rec = self.inner.lock().unwrap();
        let file = rewrite(&rec.path, &rec.meta, &[])?;
        rec.file = file;
        rec.cursor = 0;
        rec.disabled = false;
        Ok(())
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
    /// was rewritten (compaction, /clear recovery), so rewrite the file.
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
            let new = &messages[rec.cursor..];
            append(&mut rec.file, new)
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
    let mut file = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&path)?;
    write_header(&mut file, meta)?;
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

fn read_header(path: &Path) -> Option<SessionMeta> {
    let file = File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    serde_json::from_str(&first).ok()
}

#[cfg(test)]
#[path = "session/tests.rs"]
mod tests;
