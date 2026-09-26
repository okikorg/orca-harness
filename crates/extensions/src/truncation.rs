//! Truncation extension: caps oversized tool outputs in `after_tool`
//! before they reach the context (protecting the window) or a downstream
//! stream with a hard line cap. String fields over the limit are trimmed
//! head-and-tail with an elision marker; the result stays valid JSON.
//!
//! Pair it with a [`TruncationStore`] to keep the full original of every
//! truncated result, retrievable by the model through
//! [`ReadToolResultTool`](crate::ReadToolResultTool): truncated outputs
//! gain a `_readFull` hint carrying the `callId` to pass to
//! `read_tool_result`. The store is byte-budgeted and evicts oldest-first.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use orca_harness_core::{Extension, ExtensionError, Subscriptions, ToolCall, ToolResult};

/// Tool name of the paired reader; its outputs are exempt from truncation
/// (it enforces its own slice cap) and from being stored again.
pub(crate) const READ_TOOL_RESULT: &str = "read_tool_result";

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredEntry {
    call_id: String,
    tool_name: String,
    full: String,
}

struct StoreEntry {
    tool_name: String,
    /// Serialized JSON of the original, untruncated output.
    full: Arc<str>,
}

#[derive(Default)]
struct StoreInner {
    entries: HashMap<String, StoreEntry>,
    /// Insertion order for FIFO eviction.
    order: VecDeque<String>,
    bytes: usize,
}

/// Retains the full originals of truncated tool results, keyed by call
/// id, within a byte budget (oldest evicted first). Cheap to clone.
#[derive(Clone)]
pub struct TruncationStore {
    inner: Arc<Mutex<StoreInner>>,
    budget: usize,
}

impl Default for TruncationStore {
    fn default() -> Self {
        Self::new(16 * 1024 * 1024)
    }
}

impl TruncationStore {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(StoreInner::default())),
            budget: budget_bytes,
        }
    }

    /// Load a store previously written by [`save`](Self::save).
    pub fn load(path: &std::path::Path, budget_bytes: usize) -> std::io::Result<Self> {
        let store = Self::new(budget_bytes);
        if !path.exists() {
            return Ok(store);
        }
        let bytes = std::fs::read(path)?;
        let entries: Vec<StoredEntry> = serde_json::from_slice(&bytes)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        for entry in entries {
            store.insert(&entry.call_id, &entry.tool_name, entry.full);
        }
        Ok(store)
    }

    /// Persist the retained originals atomically as a session sidecar.
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let entries = {
            let inner = self.inner.lock().unwrap();
            inner
                .order
                .iter()
                .filter_map(|call_id| {
                    inner.entries.get(call_id).map(|entry| StoredEntry {
                        call_id: call_id.clone(),
                        tool_name: entry.tool_name.clone(),
                        full: entry.full.to_string(),
                    })
                })
                .collect::<Vec<_>>()
        };
        let bytes = serde_json::to_vec(&entries).map_err(std::io::Error::other)?;
        let temp = path.with_extension("recovery.tmp");
        std::fs::write(&temp, bytes)?;
        std::fs::rename(temp, path)
    }

    /// Create an independent snapshot for a forked session.
    pub fn snapshot(&self) -> Self {
        let snapshot = Self::new(self.budget);
        let entries = {
            let inner = self.inner.lock().unwrap();
            inner
                .order
                .iter()
                .filter_map(|call_id| {
                    inner.entries.get(call_id).map(|entry| {
                        (
                            call_id.clone(),
                            entry.tool_name.clone(),
                            entry.full.to_string(),
                        )
                    })
                })
                .collect::<Vec<_>>()
        };
        for (call_id, tool_name, full) in entries {
            snapshot.insert(&call_id, &tool_name, full);
        }
        snapshot
    }

    /// Remove all retained originals.
    pub fn clear(&self) {
        *self.inner.lock().unwrap() = StoreInner::default();
    }

    pub(crate) fn insert(&self, call_id: &str, tool_name: &str, full: String) {
        // An original larger than the whole budget is unstorable.
        if full.len() > self.budget {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if let Some(old) = inner.entries.remove(call_id) {
            inner.bytes -= old.full.len();
            inner.order.retain(|id| id != call_id);
        }
        inner.bytes += full.len();
        inner.entries.insert(
            call_id.to_string(),
            StoreEntry {
                tool_name: tool_name.to_string(),
                full: full.into(),
            },
        );
        inner.order.push_back(call_id.to_string());
        while inner.bytes > self.budget {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            if let Some(evicted) = inner.entries.remove(&oldest) {
                inner.bytes -= evicted.full.len();
            }
        }
    }

    /// The stored original for `call_id`: `(tool_name, serialized JSON)`.
    pub(crate) fn get(&self, call_id: &str) -> Option<(String, Arc<str>)> {
        let inner = self.inner.lock().unwrap();
        inner
            .entries
            .get(call_id)
            .map(|e| (e.tool_name.clone(), e.full.clone()))
    }
}

pub struct Truncation {
    /// Maximum length, in characters, of any single string in the output.
    max_string_chars: usize,
    store: Option<TruncationStore>,
}

impl Default for Truncation {
    fn default() -> Self {
        // Comfortably under a 4 MB downstream line cap while leaving room
        // for many strings in one result.
        Self {
            max_string_chars: 16 * 1024,
            store: None,
        }
    }
}

impl Truncation {
    pub fn new(max_string_chars: usize) -> Self {
        Self {
            max_string_chars,
            store: None,
        }
    }

    /// Retain full originals of truncated results in `store`, and stamp
    /// truncated outputs with a `_readFull` hint for `read_tool_result`.
    pub fn store(mut self, store: TruncationStore) -> Self {
        self.store = Some(store);
        self
    }

    fn needs_truncation(&self, value: &Value) -> bool {
        match value {
            Value::String(s) => s.chars().count() > self.max_string_chars,
            Value::Array(items) => items.iter().any(|v| self.needs_truncation(v)),
            Value::Object(map) => map.values().any(|v| self.needs_truncation(v)),
            _ => false,
        }
    }

    fn truncate_value(&self, value: &mut Value) -> bool {
        match value {
            Value::String(s) => {
                let len = s.chars().count();
                if len > self.max_string_chars {
                    let keep = self.max_string_chars / 2;
                    let head: String = s.chars().take(keep).collect();
                    let tail: String = s.chars().skip(len - keep).collect();
                    let elided = len - 2 * keep;
                    *s = format!("{head}\n… [{elided} chars elided] …\n{tail}");
                    true
                } else {
                    false
                }
            }
            Value::Array(items) => {
                let mut any = false;
                for item in items {
                    any |= self.truncate_value(item);
                }
                any
            }
            Value::Object(map) => {
                let mut any = false;
                for (_, v) in map.iter_mut() {
                    any |= self.truncate_value(v);
                }
                any
            }
            _ => false,
        }
    }
}

#[async_trait]
impl Extension for Truncation {
    fn name(&self) -> &str {
        "truncation"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().after_tool()
    }

    async fn after_tool(
        &self,
        call: &ToolCall,
        mut result: ToolResult,
    ) -> Result<ToolResult, ExtensionError> {
        // The reader's slices are sized by explicit request; re-truncating
        // them would make full outputs permanently unreachable.
        if result.tool_name == READ_TOOL_RESULT {
            return Ok(result);
        }
        // Image data is sent as images, not text: never cut or store it.
        let images = crate::tool_images::take(&mut result.output);
        // Capture the original before mutating, but only when something
        // will actually be trimmed and there is a store to keep it.
        let full = match &self.store {
            Some(_) if self.needs_truncation(&result.output) => {
                serde_json::to_string(&result.output).ok()
            }
            _ => None,
        };
        let truncated = self.truncate_value(&mut result.output);
        if truncated {
            let stored = match (&self.store, full) {
                (Some(store), Some(full)) => {
                    store.insert(&call.id, &result.tool_name, full);
                    true
                }
                _ => false,
            };
            if let Value::Object(map) = &mut result.output {
                map.insert("_truncated".into(), Value::Bool(true));
                if stored {
                    map.insert(
                        "_readFull".into(),
                        Value::String(format!(
                            "full output available: call {READ_TOOL_RESULT} with callId \"{}\"",
                            call.id
                        )),
                    );
                }
            }
        }
        crate::tool_images::restore(&mut result.output, images);
        Ok(result)
    }
}
