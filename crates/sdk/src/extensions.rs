use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::{Model, ModelError, ToolCall, ToolError};
use orca_harness_extensions::{CompactReport, LongSessionConfig, ModelGate, RetryModel, ToolRetry};
use orca_harness_tools::retry::{data_failure, excludes_delegation, retryable_error};
use serde_json::Value;

pub type CompactCallback = std::sync::Arc<dyn Fn(CompactReport) + Send + Sync>;

/// A caller rule marking an `Ok` tool result as a failure to retry.
pub type OkRetryPredicate = Arc<dyn Fn(&ToolCall, &Value) -> bool + Send + Sync>;
/// A caller rule deciding whether a returned tool error is safe to retry.
pub type ErrorRetryPredicate = Arc<dyn Fn(&ToolCall, &ToolError) -> bool + Send + Sync>;
/// Reads the live model retry policy at each model failure.
pub type LiveModelRetry = Arc<dyn Fn() -> orca_harness_extensions::ModelRetryConfig + Send + Sync>;
/// Observes the next model attempt: `(attempt, max_attempts, error)`.
pub type ModelRetryNotice = Arc<dyn Fn(u32, Option<u32>, &ModelError) + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub struct TruncationConfig {
    pub max_string_chars: usize,
    pub store_budget_bytes: usize,
    pub expose_reader_tool: bool,
}

impl Default for TruncationConfig {
    fn default() -> Self {
        Self {
            max_string_chars: 16 * 1024,
            store_budget_bytes: 16 * 1024 * 1024,
            expose_reader_tool: true,
        }
    }
}

/// The simple retry shape: total attempts and a fixed backoff. Accepted
/// by [`AgentBuilder::tool_retry`](crate::AgentBuilder::tool_retry),
/// [`AgentBuilder::model_retry`](crate::AgentBuilder::model_retry), and
/// [`SubagentConfig::tool_retry`](crate::SubagentConfig::tool_retry); the
/// first two convert it to [`ToolRetryConfig`] / [`ModelRetryConfig`] with
/// their defaults.
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    pub attempts: u32,
    pub backoff_ms: u64,
}

impl RetryConfig {
    pub fn attempts(attempts: u32) -> Self {
        Self {
            attempts: attempts.max(1),
            backoff_ms: 250,
        }
    }

    pub fn backoff_ms(mut self, backoff_ms: u64) -> Self {
        self.backoff_ms = backoff_ms;
        self
    }
}

/// How a run retries the agent's own tool calls (one `ToolRetry` per run,
/// wrapping every tool the model calls).
///
/// What counts as a failure: a returned error, and, with
/// [`retry_data_failures`](Self::retry_data_failures) on (the default), an
/// `Ok` result the core tools report as a failure in data (`shell` /
/// `process` with `success: false`, `web_fetch` with a 5xx status; see
/// [`orca_harness_tools::retry::data_failure`]). When attempts run out
/// the last real output is returned as-is, so the model sees the actual
/// failure.
///
/// What is excluded: with [`exclude_non_idempotent`](Self::exclude_non_idempotent)
/// on (the default), errors from `write_file`, `edit_file`, `multi_edit`,
/// and `apply_patch` are never replayed (their failures are deterministic
/// or may repeat a half-applied mutation), and neither are `subagent`
/// control actions (`list`, `cancel`, ...); see
/// [`orca_harness_tools::retry::retryable_error`].
///
/// Custom predicates compose with the built-ins rather than replace them:
/// an error retries only when the built-in rule *and*
/// [`retry_error_when`](Self::retry_error_when) both accept it; an `Ok`
/// result retries when the built-in data rule *or*
/// [`retry_ok_when`](Self::retry_ok_when) flags it.
///
/// Delegation: when the agent's subagents retry their own tool calls
/// ([`SubagentConfig::tool_retry`](crate::SubagentConfig::tool_retry)),
/// this layer never replays a `subagent` run or a `workflow` run, so a
/// child's failure is retried once, inside the child. Without child retry
/// a failed `subagent` run is retried here like any other tool error.
#[derive(Clone)]
pub struct ToolRetryConfig {
    pub attempts: u32,
    pub backoff_ms: u64,
    pub retry_data_failures: bool,
    pub exclude_non_idempotent: bool,
    retry_ok_when: Option<OkRetryPredicate>,
    retry_error_when: Option<ErrorRetryPredicate>,
}

impl ToolRetryConfig {
    /// `attempts` total tries (at least one) with the default 250 ms
    /// backoff and both built-in classifiers on.
    pub fn attempts(attempts: u32) -> Self {
        RetryConfig::attempts(attempts).into()
    }

    pub fn backoff_ms(mut self, backoff_ms: u64) -> Self {
        self.backoff_ms = backoff_ms;
        self
    }

    /// Also treat the core tools' data failures as failed attempts.
    pub fn retry_data_failures(mut self, enabled: bool) -> Self {
        self.retry_data_failures = enabled;
        self
    }

    /// Never replay native file mutations or subagent control actions.
    pub fn exclude_non_idempotent(mut self, enabled: bool) -> Self {
        self.exclude_non_idempotent = enabled;
        self
    }

    /// Flag further `Ok` results as failures (composed with `or`).
    pub fn retry_ok_when(
        mut self,
        rule: impl Fn(&ToolCall, &Value) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.retry_ok_when = Some(Arc::new(rule));
        self
    }

    /// Narrow which errors retry (composed with `and`).
    pub fn retry_error_when(
        mut self,
        rule: impl Fn(&ToolCall, &ToolError) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.retry_error_when = Some(Arc::new(rule));
        self
    }

    /// The per-run extension. `children_retry` is whether the agent's
    /// subagents retry inside, in which case delegation calls are left
    /// to them (neither their errors nor their results are retried here).
    pub(crate) fn extension(&self, children_retry: bool) -> ToolRetry {
        let mut retry =
            ToolRetry::new(self.attempts).backoff(Duration::from_millis(self.backoff_ms));
        let builtin_ok = self.retry_data_failures;
        let custom_ok = self.retry_ok_when.clone();
        if builtin_ok || custom_ok.is_some() {
            retry = retry.retry_ok_when(move |call, out| {
                if children_retry && excludes_delegation(call) {
                    return false;
                }
                (builtin_ok && data_failure(call, out))
                    || custom_ok.as_ref().is_some_and(|rule| rule(call, out))
            });
        }
        let builtin_error = self.exclude_non_idempotent;
        let custom_error = self.retry_error_when.clone();
        if builtin_error || custom_error.is_some() || children_retry {
            retry = retry.retry_error_when(move |call, error| {
                if children_retry && excludes_delegation(call) {
                    return false;
                }
                (!builtin_error || retryable_error(call, error))
                    && custom_error.as_ref().is_none_or(|rule| rule(call, error))
            });
        }
        retry
    }
}

impl From<RetryConfig> for ToolRetryConfig {
    fn from(config: RetryConfig) -> Self {
        Self {
            attempts: config.attempts.max(1),
            backoff_ms: config.backoff_ms,
            retry_data_failures: true,
            exclude_non_idempotent: true,
            retry_ok_when: None,
            retry_error_when: None,
        }
    }
}

/// How the agent's model retries transient failures (transport errors,
/// provider 408/429/5xx with their `Retry-After`, output limits; permanent
/// quota and authentication errors are never retried).
///
/// The model is wrapped once, when the agent is built, so every session
/// and every subagent that inherits the agent's model shares that one
/// retry layer: model retry is never nested. Models routed to subagents
/// through [`SubagentConfig::model`](crate::SubagentConfig::model) are
/// separate and not wrapped.
///
/// This is the configuration; the live policy a
/// [`live`](Self::live) closure returns is
/// [`integrations::ModelRetryConfig`](crate::integrations::ModelRetryConfig),
/// read at each failure so a host's settings edits reach wrappers already
/// built. With a live closure, `attempts`, `backoff_ms`, and
/// `max_backoff_ms` here are only the initial values.
#[derive(Clone)]
pub struct ModelRetryConfig {
    /// Total tries; `None` retries until success, a permanent error, or
    /// cancellation.
    pub attempts: Option<u32>,
    /// Base exponential backoff; provider timing remains a minimum wait.
    pub backoff_ms: u64,
    /// Cap on the backoff ceiling; `None` leaves it unbounded.
    pub max_backoff_ms: Option<u64>,
    gate: Option<ModelGate>,
    live: Option<LiveModelRetry>,
    on_retry: Option<ModelRetryNotice>,
}

impl Default for ModelRetryConfig {
    /// Unbounded attempts with the extension's default backoff.
    fn default() -> Self {
        Self {
            attempts: None,
            backoff_ms: orca_harness_extensions::ModelRetryConfig::default()
                .backoff
                .as_millis() as u64,
            max_backoff_ms: None,
            gate: None,
            live: None,
            on_retry: None,
        }
    }
}

impl ModelRetryConfig {
    /// `attempts` total tries (at least one) with the default 250 ms
    /// backoff.
    pub fn attempts(attempts: u32) -> Self {
        RetryConfig::attempts(attempts).into()
    }

    pub fn backoff_ms(mut self, backoff_ms: u64) -> Self {
        self.backoff_ms = backoff_ms;
        self
    }

    pub fn max_backoff_ms(mut self, max_backoff_ms: u64) -> Self {
        self.max_backoff_ms = Some(max_backoff_ms);
        self
    }

    /// Share admission and cooldown with other models on the same
    /// provider quota (see
    /// [`integrations::ModelGate`](crate::integrations::ModelGate)).
    pub fn gate(mut self, gate: ModelGate) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Read the policy at each failure instead of the fixed values here.
    pub fn live(
        mut self,
        policy: impl Fn() -> orca_harness_extensions::ModelRetryConfig + Send + Sync + 'static,
    ) -> Self {
        self.live = Some(Arc::new(policy));
        self
    }

    /// Observe each upcoming attempt (numbered from two) with its cap and
    /// the error that caused it.
    pub fn on_retry(
        mut self,
        callback: impl Fn(u32, Option<u32>, &ModelError) + Send + Sync + 'static,
    ) -> Self {
        self.on_retry = Some(Arc::new(callback));
        self
    }

    pub(crate) fn wrap(&self, model: Arc<dyn Model>) -> RetryModel<Arc<dyn Model>> {
        let backoff = Duration::from_millis(self.backoff_ms);
        let mut retry = RetryModel::new(model, self.attempts)
            .backoff(backoff)
            .retry_delay(orca_harness_model_providers::http_error::retry_delay);
        match (&self.live, self.max_backoff_ms) {
            (Some(live), _) => {
                let live = live.clone();
                retry = retry.config(move || live());
            }
            (None, Some(max_backoff_ms)) => {
                let snapshot = orca_harness_extensions::ModelRetryConfig {
                    max_attempts: self.attempts,
                    backoff,
                    max_backoff: Some(Duration::from_millis(max_backoff_ms)),
                };
                retry = retry.config(move || snapshot);
            }
            (None, None) => {}
        }
        if let Some(gate) = &self.gate {
            retry = retry.gate(gate.clone());
        }
        if let Some(notice) = &self.on_retry {
            let notice = notice.clone();
            retry = retry.on_retry(move |attempt, max, error| notice(attempt, max, error));
        }
        retry
    }
}

impl From<RetryConfig> for ModelRetryConfig {
    fn from(config: RetryConfig) -> Self {
        Self {
            attempts: Some(config.attempts.max(1)),
            backoff_ms: config.backoff_ms,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub enum Compaction {
    #[default]
    Manual,
    Automatic(LongSessionConfig),
    Off,
}

#[derive(Clone)]
pub(crate) struct ExtensionConfig {
    pub events: bool,
    pub usage: bool,
    pub truncation: Option<TruncationConfig>,
    pub retry: Option<ToolRetryConfig>,
    pub model_retry: Option<ModelRetryConfig>,
    pub compaction: Compaction,
    pub on_compact: Option<CompactCallback>,
}

impl Default for ExtensionConfig {
    fn default() -> Self {
        Self {
            events: true,
            usage: true,
            truncation: Some(TruncationConfig::default()),
            retry: None,
            model_retry: None,
            compaction: Compaction::Manual,
            on_compact: None,
        }
    }
}
