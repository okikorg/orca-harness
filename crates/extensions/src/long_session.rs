//! Model-aware context compaction for long-running sessions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use orca_harness_core::{Context, Extension, ExtensionError, ModelResponse, Subscriptions};

use crate::compact::estimated_context_tokens;
use crate::{compact, CompactConfig, CompactReport, TruncationStore};

/// Shared, dynamically replaceable capacity of the active model.
#[derive(Clone, Default)]
pub struct ContextCapacity {
    state: Arc<Mutex<CapacityState>>,
}

#[derive(Default)]
struct CapacityState {
    tokens: Option<u64>,
    revision: u64,
}

impl ContextCapacity {
    pub fn new(tokens: Option<u64>) -> Self {
        let capacity = Self::default();
        capacity.set(tokens);
        capacity
    }

    pub fn get(&self) -> Option<u64> {
        self.state.lock().unwrap().tokens
    }

    pub fn set(&self, tokens: Option<u64>) {
        self.state.lock().unwrap().tokens = tokens;
    }

    /// Clear stale capacity and return a revision for one asynchronous probe.
    pub fn begin_update(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.tokens = None;
        state.revision = state.revision.wrapping_add(1);
        state.revision
    }

    /// Apply a probe result only if no newer model selection has started.
    pub fn finish_update(&self, revision: u64, tokens: Option<u64>) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.revision != revision {
            return false;
        }
        state.tokens = tokens;
        true
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LongSessionConfig {
    /// Compact after the last model step occupied this percentage of its window.
    pub compact_at_percent: u8,
    /// Percentage of the model window retained as a verbatim tail.
    pub tail_percent: u8,
}

impl Default for LongSessionConfig {
    fn default() -> Self {
        Self {
            compact_at_percent: 80,
            tail_percent: 20,
        }
    }
}

type CompactCallback = Arc<dyn Fn(CompactReport) + Send + Sync>;

/// Keeps a session below the selected model's discovered context capacity.
pub struct LongSession {
    capacity: ContextCapacity,
    store: TruncationStore,
    config: LongSessionConfig,
    last_context_tokens: AtomicU64,
    on_compact: Option<CompactCallback>,
}

impl LongSession {
    pub fn new(capacity: ContextCapacity, store: TruncationStore) -> Self {
        Self {
            capacity,
            store,
            config: LongSessionConfig::default(),
            last_context_tokens: AtomicU64::new(0),
            on_compact: None,
        }
    }

    pub fn config(mut self, config: LongSessionConfig) -> Self {
        self.config = config;
        self
    }

    pub fn on_compact(mut self, callback: impl Fn(CompactReport) + Send + Sync + 'static) -> Self {
        self.on_compact = Some(Arc::new(callback));
        self
    }

    fn threshold(&self, capacity: u64) -> u64 {
        capacity.saturating_mul(u64::from(self.config.compact_at_percent)) / 100
    }
}

#[async_trait]
impl Extension for LongSession {
    fn name(&self) -> &str {
        "long-session"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_model().after_model()
    }

    async fn before_model(&self, context: &mut Context) -> Result<(), ExtensionError> {
        let Some(capacity) = self.capacity.get() else {
            return Ok(());
        };
        let reported = self.last_context_tokens.load(Ordering::Relaxed);
        let occupied = if reported == 0 {
            estimated_context_tokens(context)
        } else {
            reported
        };
        if occupied < self.threshold(capacity) {
            return Ok(());
        }

        let tail_budget_tokens = capacity
            .saturating_mul(u64::from(self.config.tail_percent))
            .checked_div(100)
            .unwrap_or(0);
        let tail_budget_tokens = usize::try_from(tail_budget_tokens).unwrap_or(usize::MAX);
        match compact(context, &self.store, &CompactConfig { tail_budget_tokens }) {
            Ok(report) => {
                self.last_context_tokens
                    .store(report.est_tokens_after as u64, Ordering::Relaxed);
                if let Some(callback) = &self.on_compact {
                    callback(report);
                }
            }
            // A small context can cross the provider-reported threshold due
            // to output reservation or accounting differences. Retrying on
            // the next step is harmless and avoids making compaction fatal.
            Err(crate::CompactError::NothingToCompact) => {}
        }
        Ok(())
    }

    async fn after_model(
        &self,
        _context: &mut Context,
        response: &ModelResponse,
    ) -> Result<(), ExtensionError> {
        if let Some(usage) = response.usage() {
            self.last_context_tokens
                .store(usage.context_tokens(), Ordering::Relaxed);
        }
        Ok(())
    }
}
