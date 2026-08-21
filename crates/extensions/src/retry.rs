//! Retry: two independent pieces.
//!
//! - [`ToolRetry`] is an `around_tool` extension that re-invokes the tool
//!   on failure, with fixed backoff. It relies on `Next` being `Copy` so
//!   the continuation can be called more than once. `Err` results always
//!   retry; [`ToolRetry::retry_ok_when`] extends that to failures a tool
//!   reports *as data* (a nonzero shell exit, an HTTP 5xx) — so
//!   "the tool ran, the operation failed" is retried too.
//! - [`RetryModel`] is a `Model` decorator that retries transient model
//!   failures. Model retries need no kernel hook — a wrapping `Model` is
//!   the natural home — so it lives here beside the tool retry for
//!   discoverability.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use orca_harness_core::{
    Context, Extension, Model, ModelError, ModelResponse, Next, Subscriptions, ToolCall,
    ToolContext, ToolError, ToolSchema,
};

/// A predicate marking an `Ok` result as a failure for retry purposes.
/// `call` is passed so one policy can branch per tool (shell exit code,
/// HTTP status, ...).
type RetryRule = Arc<dyn Fn(&ToolCall, &Value) -> bool + Send + Sync>;

/// Retries a failing tool up to `max_attempts` total tries.
pub struct ToolRetry {
    max_attempts: u32,
    backoff: Duration,
    /// Extra failures expressed as *successful* results. `Err` results are
    /// always retried; with this set, an `Ok` value the rule rejects is
    /// treated as a failed attempt too. `None` keeps retry-on-Err-only
    /// behavior.
    retry_ok: Option<RetryRule>,
}

impl ToolRetry {
    /// `max_attempts` is the total number of tries (>= 1). One retry means
    /// `max_attempts = 2`.
    pub fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            backoff: Duration::from_millis(50),
            retry_ok: None,
        }
    }

    pub fn backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Also retry `Ok` results that `rule` flags as failures (e.g. a
    /// shell result with `success == false`, or an HTTP status >= 500).
    /// When attempts are exhausted the last result — however it was
    /// classified — is returned as-is, so the model still sees the actual
    /// output instead of a synthetic error.
    pub fn retry_ok_when(
        mut self,
        rule: impl Fn(&ToolCall, &Value) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.retry_ok = Some(Arc::new(rule));
        self
    }
}

#[async_trait]
impl Extension for ToolRetry {
    fn name(&self) -> &str {
        "tool-retry"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().around_tool()
    }

    async fn around_tool<'a>(
        &self,
        call: &ToolCall,
        input: Value,
        ctx: &ToolContext,
        next: Next<'a>,
    ) -> Result<Value, ToolError> {
        let mut last_err = None;
        let mut last_value = None;
        for attempt in 1..=self.max_attempts {
            // `next` is Copy, so each attempt gets a fresh continuation.
            match next.run(input.clone()).await {
                Ok(value) => {
                    // An `Ok` result only counts as a success when no rule
                    // rejects it; otherwise treat the attempt as failed and
                    // retry, keeping the raw output for the final attempt.
                    if self
                        .retry_ok
                        .as_ref()
                        .is_some_and(|rule| rule(call, &value))
                    {
                        last_value = Some(value);
                        if attempt == self.max_attempts {
                            // Exhausted: hand the last real output back so
                            // the model sees the actual failure, not a
                            // synthetic one.
                            break;
                        }
                        if ctx.cancellation.is_cancelled() {
                            break;
                        }
                        tokio::time::sleep(self.backoff).await;
                        continue;
                    }
                    return Ok(value);
                }
                Err(err) => {
                    last_err = Some(err);
                    if attempt < self.max_attempts {
                        // Give up early if the run is being torn down.
                        if ctx.cancellation.is_cancelled() {
                            break;
                        }
                        tokio::time::sleep(self.backoff).await;
                    }
                }
            }
        }
        match last_value {
            Some(value) => Ok(value),
            None => Err(last_err.unwrap_or_else(|| ToolError::msg("tool retry: no attempts made"))),
        }
    }
}

/// Wraps a [`Model`], retrying transient [`ModelError::Request`] failures.
/// `InvalidResponse` errors are not retried — they are deterministic.
pub struct RetryModel<M: Model> {
    inner: M,
    max_attempts: u32,
    backoff: Duration,
}

impl<M: Model> RetryModel<M> {
    pub fn new(inner: M, max_attempts: u32) -> Self {
        Self {
            inner,
            max_attempts: max_attempts.max(1),
            backoff: Duration::from_millis(200),
        }
    }

    pub fn backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }
}

#[async_trait]
impl<M: Model> Model for RetryModel<M> {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        let mut last_err = None;
        for attempt in 1..=self.max_attempts {
            match self.inner.generate(context, tools).await {
                Ok(response) => return Ok(response),
                // Deterministic; retrying will not help.
                Err(err @ ModelError::InvalidResponse(_)) => return Err(err),
                Err(err) => {
                    last_err = Some(err);
                    if attempt < self.max_attempts {
                        tokio::time::sleep(self.backoff).await;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| ModelError::Request("retry: no attempts made".into())))
    }
}
