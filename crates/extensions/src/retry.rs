//! Retry: two independent pieces.
//!
//! - [`ToolRetry`] is an `around_tool` extension that re-invokes the tool
//!   on failure, with fixed backoff. It relies on `Next` being `Copy` so
//!   the continuation can be called more than once. Errors retry by default;
//!   [`ToolRetry::retry_error_when`] can narrow them, while
//!   [`ToolRetry::retry_ok_when`] extends retries to failures a tool reports
//!   *as data* (a nonzero shell exit, an HTTP 5xx).
//! - [`RetryModel`] is a `Model` decorator that retries transient model
//!   failures. Model retries need no kernel hook — a wrapping `Model` is
//!   the natural home — so it lives here beside the tool retry for
//!   discoverability.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use orca_harness_core::{
    Context, DeltaSink, Extension, Model, ModelError, ModelResponse, Next, Subscriptions, ToolCall,
    ToolContext, ToolError, ToolSchema,
};

/// A predicate marking an `Ok` result as a failure for retry purposes.
/// `call` is passed so one policy can branch per tool (shell exit code,
/// HTTP status, ...).
type RetryRule = Arc<dyn Fn(&ToolCall, &Value) -> bool + Send + Sync>;

/// A predicate deciding whether a returned tool error is safe to retry.
type ErrorRetryRule = Arc<dyn Fn(&ToolCall, &ToolError) -> bool + Send + Sync>;

/// Called immediately before another model attempt begins.
type ModelRetryNotice = Arc<dyn Fn(u32, u32, &ModelError) + Send + Sync>;

/// Retries a failing tool up to `max_attempts` total tries.
pub struct ToolRetry {
    max_attempts: u32,
    backoff: Duration,
    /// Extra failures expressed as *successful* results. With this set, an
    /// `Ok` value the rule rejects is treated as a failed attempt too. `None`
    /// keeps retry-on-Err-only behavior.
    retry_ok: Option<RetryRule>,
    /// Optional restriction for returned errors. `None` preserves the
    /// historical behavior of retrying every `Err`.
    retry_error: Option<ErrorRetryRule>,
}

impl ToolRetry {
    /// `max_attempts` is the total number of tries (>= 1). One retry means
    /// `max_attempts = 2`.
    pub fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            backoff: Duration::from_millis(50),
            retry_ok: None,
            retry_error: None,
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

    /// Retry returned errors only when `rule` accepts them. This is useful for
    /// excluding deterministic validation errors or non-idempotent mutations
    /// while retaining the default retry-all behavior for other tools.
    pub fn retry_error_when(
        mut self,
        rule: impl Fn(&ToolCall, &ToolError) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.retry_error = Some(Arc::new(rule));
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
                    if self
                        .retry_error
                        .as_ref()
                        .is_some_and(|rule| !rule(call, &err))
                    {
                        return Err(err);
                    }
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
    on_retry: Option<ModelRetryNotice>,
}

impl<M: Model> RetryModel<M> {
    pub fn new(inner: M, max_attempts: u32) -> Self {
        Self {
            inner,
            max_attempts: max_attempts.max(1),
            backoff: Duration::from_millis(200),
            on_retry: None,
        }
    }

    pub fn backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Observe a retry without coupling the model layer to a particular UI.
    /// `attempt` is the upcoming attempt number, starting at two.
    pub fn on_retry(
        mut self,
        callback: impl Fn(u32, u32, &ModelError) + Send + Sync + 'static,
    ) -> Self {
        self.on_retry = Some(Arc::new(callback));
        self
    }

    fn notify_retry(&self, next_attempt: u32, error: &ModelError) {
        if let Some(callback) = &self.on_retry {
            callback(next_attempt, self.max_attempts, error);
        }
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
                Err(err @ (ModelError::InvalidResponse(_) | ModelError::Authentication(_))) => {
                    return Err(err)
                }
                Err(err) => {
                    if attempt < self.max_attempts {
                        self.notify_retry(attempt + 1, &err);
                        tokio::time::sleep(self.backoff).await;
                    }
                    last_err = Some(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| ModelError::Request("retry: no attempts made".into())))
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        let mut last_err = None;
        for attempt in 1..=self.max_attempts {
            match self.inner.generate_streaming(context, tools, sink).await {
                Ok(response) => return Ok(response),
                // Deterministic; retrying will not help.
                Err(err @ (ModelError::InvalidResponse(_) | ModelError::Authentication(_))) => {
                    return Err(err)
                }
                Err(err) => {
                    if attempt < self.max_attempts {
                        self.notify_retry(attempt + 1, &err);
                        tokio::time::sleep(self.backoff).await;
                    }
                    last_err = Some(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| ModelError::Request("retry: no attempts made".into())))
    }
}
