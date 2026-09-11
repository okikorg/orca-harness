//! Session-scoped stage outputs. Nothing leaves the process.
//!
//! The store outlives any one [`WorkflowTool`](super::WorkflowTool): the host
//! rebuilds its tools whenever the model changes, and a run admitted before
//! that switch must still replay afterwards. Hosts therefore create one store
//! per session, alongside the subagent manager whose spawn sequence names the
//! runs, and clone it into every rebuild.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

struct Record {
    key: String,
    answer: String,
}

#[derive(Default)]
struct Run {
    stages: HashMap<String, Record>,
    outcome: Option<serde_json::Value>,
}

/// A shared handle: cloning shares the runs, it does not copy them.
#[derive(Clone, Default)]
pub struct WorkflowStore(Arc<Mutex<HashMap<u64, Run>>>);

impl WorkflowStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget every run. Hosts call this when they replace the conversation,
    /// which has already cancelled the runs those outputs belonged to.
    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    /// How many runs this session is holding outputs for.
    pub fn runs(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    pub(super) fn create(&self, run: u64) {
        self.0.lock().unwrap().entry(run).or_default();
    }

    /// Whether this session admitted `run`. Run ids come from the manager's
    /// session-wide spawn sequence, so they are unique while the store lives.
    pub(super) fn exists(&self, run: u64) -> bool {
        self.0.lock().unwrap().contains_key(&run)
    }

    pub(super) fn write(&self, run: u64, stage: &str, key: String, answer: String) {
        self.0
            .lock()
            .unwrap()
            .entry(run)
            .or_default()
            .stages
            .insert(stage.to_string(), Record { key, answer });
    }

    pub(super) fn output(&self, run: u64, stage: &str) -> Option<String> {
        self.read(run, stage, None)
    }

    pub(super) fn replay(&self, run: u64, stage: &str, key: &str) -> Option<String> {
        self.read(run, stage, Some(key))
    }

    fn read(&self, run: u64, stage: &str, key: Option<&str>) -> Option<String> {
        let runs = self.0.lock().unwrap();
        let record = runs.get(&run)?.stages.get(stage)?;
        key.is_none_or(|key| record.key == key)
            .then(|| record.answer.clone())
    }

    /// The terminal outcome the runtime recorded for `run`, once it finished.
    pub(super) fn stored_outcome(&self, run: u64) -> Option<serde_json::Value> {
        self.0.lock().unwrap().get(&run)?.outcome.clone()
    }

    pub(super) fn set_outcome(&self, run: u64, value: &serde_json::Value) {
        self.0.lock().unwrap().entry(run).or_default().outcome = Some(value.clone());
    }
}
