//! Tool and model retry options: configuration over the extensions
//! crate's `ToolRetry` and `RetryModel` builders, with the tools crate's
//! classifiers composed in.

use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::{Model, ModelError, ToolCall, ToolError};
use orca_harness_extensions::{ModelGate, RetryModel, ToolRetry};
use orca_harness_tools::retry::{data_failure, excludes_delegation, retryable_error};
use orca_harness_tools::SubagentDepth;
use serde_json::Value;

use super::RetryConfig;

type OkRetryPredicate = Arc<dyn Fn(&ToolCall, &Value) -> bool + Send + Sync>;
type ErrorRetryPredicate = Arc<dyn Fn(&ToolCall, &ToolError) -> bool + Send + Sync>;
type LiveModelRetry = Arc<dyn Fn() -> orca_harness_extensions::ModelRetryConfig + Send + Sync>;
type ModelRetryNotice = Arc<dyn Fn(u32, Option<u32>, &ModelError) + Send + Sync>;

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
/// Delegation: while the agent's subagents retry their own tool calls
/// (the live [`SubagentDepth::tool_attempts`] is above one, whether set by
/// [`SubagentConfig::tool_retry`](crate::SubagentConfig::tool_retry) or
/// through the settings handle), this layer never replays a `subagent`
/// run or a `workflow` run, so a child's failure is retried once, inside
/// the child. The check is made per call, so a live edit takes effect at
/// the next tool call. While children do not retry, a failed `subagent`
/// run is retried here like any other tool error.
#[derive(Clone)]
pub struct ToolRetryOptions {
    pub attempts: u32,
    pub backoff_ms: u64,
    pub retry_data_failures: bool,
    pub exclude_non_idempotent: bool,
    retry_ok_when: Option<OkRetryPredicate>,
    retry_error_when: Option<ErrorRetryPredicate>,
}

impl ToolRetryOptions {
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

    /// The per-run extension. `subagents` is the agent's live subagent
    /// settings handle, when it has subagents: while its `tool_attempts`
    /// is above one, children retry inside and delegation calls are left
    /// to them (neither their errors nor their results are retried here).
    pub(crate) fn extension(&self, subagents: Option<SubagentDepth>) -> ToolRetry {
        let mut retry =
            ToolRetry::new(self.attempts).backoff(Duration::from_millis(self.backoff_ms));
        let children_retry: Arc<dyn Fn(&ToolCall) -> bool + Send + Sync> = Arc::new(move |call| {
            subagents
                .as_ref()
                .is_some_and(|settings| settings.tool_attempts() > 1)
                && excludes_delegation(call)
        });
        let builtin_ok = self.retry_data_failures;
        let custom_ok = self.retry_ok_when.clone();
        if builtin_ok || custom_ok.is_some() {
            let children_retry = children_retry.clone();
            retry = retry.retry_ok_when(move |call, out| {
                !children_retry(call)
                    && ((builtin_ok && data_failure(call, out))
                        || custom_ok.as_ref().is_some_and(|rule| rule(call, out)))
            });
        }
        let builtin_error = self.exclude_non_idempotent;
        let custom_error = self.retry_error_when.clone();
        retry.retry_error_when(move |call, error| {
            !children_retry(call)
                && (!builtin_error || retryable_error(call, error))
                && custom_error.as_ref().is_none_or(|rule| rule(call, error))
        })
    }
}

impl From<RetryConfig> for ToolRetryOptions {
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
/// These are the options; the policy a [`live`](Self::live) closure
/// returns is the extensions crate's
/// [`ModelRetryConfig`](crate::integrations::ModelRetryConfig), read at
/// each failure so a host's settings edits reach wrappers already built.
/// With a live closure, `attempts`, `backoff_ms`, and `max_backoff_ms`
/// here are ignored: the closure's snapshot replaces them at the first
/// failure.
#[derive(Clone)]
pub struct ModelRetryOptions {
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

impl Default for ModelRetryOptions {
    /// Unbounded attempts with the extensions crate's default backoff
    /// (200 ms).
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

impl ModelRetryOptions {
    /// `attempts` total tries (at least one) with the default 200 ms
    /// backoff. (A converted [`RetryConfig`] carries its own 250 ms.)
    pub fn attempts(attempts: u32) -> Self {
        Self {
            attempts: Some(attempts.max(1)),
            ..Self::default()
        }
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

impl From<RetryConfig> for ModelRetryOptions {
    fn from(config: RetryConfig) -> Self {
        Self {
            attempts: Some(config.attempts.max(1)),
            backoff_ms: config.backoff_ms,
            ..Self::default()
        }
    }
}
