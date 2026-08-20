//! Retry: two independent pieces.
//!
//! - [`ToolRetry`] is an `around_tool` extension that re-invokes the tool
//!   on failure, with fixed backoff. It relies on `Next` being `Copy` so
//!   the continuation can be called more than once.
//! - [`RetryModel`] is a `Model` decorator that retries transient model
//!   failures. Model retries need no kernel hook — a wrapping `Model` is
//!   the natural home — so it lives here beside the tool retry for
//!   discoverability.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use orca_harness_core::{
    Context, Extension, Model, ModelError, ModelResponse, Next, Subscriptions, ToolCall,
    ToolContext, ToolError, ToolSchema,
};

/// Retries a failing tool up to `max_attempts` total tries.
pub struct ToolRetry {
    max_attempts: u32,
    backoff: Duration,
}

impl ToolRetry {
    /// `max_attempts` is the total number of tries (>= 1). One retry means
    /// `max_attempts = 2`.
    pub fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            backoff: Duration::from_millis(50),
        }
    }

    pub fn backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
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
        _call: &ToolCall,
        input: Value,
        ctx: &ToolContext,
        next: Next<'a>,
    ) -> Result<Value, ToolError> {
        let mut last_err = None;
        for attempt in 1..=self.max_attempts {
            // `next` is Copy, so each attempt gets a fresh continuation.
            match next.run(input.clone()).await {
                Ok(value) => return Ok(value),
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
        Err(last_err.unwrap_or_else(|| ToolError::msg("tool retry: no attempts made")))
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
