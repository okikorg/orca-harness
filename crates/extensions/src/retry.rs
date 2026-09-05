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
type ModelRetryNotice = Arc<dyn Fn(u32, Option<u32>, &ModelError) + Send + Sync>;

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

/// Shared admission and cooldown for models drawing on the same provider quota.
/// Dropping a queued or active request releases its place/permit automatically.
#[derive(Clone)]
pub struct ModelGate {
    admission: Arc<tokio::sync::Mutex<()>>,
    active: tokio::sync::watch::Sender<usize>,
    limit: tokio::sync::watch::Receiver<usize>,
    cooldown: Arc<std::sync::Mutex<Option<tokio::time::Instant>>>,
}

struct ModelPermit<'a>(&'a tokio::sync::watch::Sender<usize>);
impl Drop for ModelPermit<'_> {
    fn drop(&mut self) {
        self.0.send_modify(|active| *active -= 1);
    }
}

impl ModelGate {
    /// None leaves concurrency unrestricted while retaining shared cooldown.
    pub fn new(concurrency: impl Into<Option<usize>>) -> Self {
        let limit = concurrency.into().map_or(0, |n| n.max(1));
        Self::from_limit(tokio::sync::watch::channel(limit).1)
    }

    /// A live limit shared by the host. Zero means unrestricted. Closing the
    /// sender retains its last value; changing it wakes queued requests.
    pub fn from_limit(limit: tokio::sync::watch::Receiver<usize>) -> Self {
        Self {
            admission: Arc::new(tokio::sync::Mutex::new(())),
            active: tokio::sync::watch::Sender::new(0),
            limit,
            cooldown: Arc::new(std::sync::Mutex::new(Some(tokio::time::Instant::now()))),
        }
    }

    async fn acquire(&self) -> ModelPermit<'_> {
        // The async mutex supplies FIFO admission without a second worker queue.
        let _admission = self.admission.lock().await;
        let mut active = self.active.subscribe();
        let mut limit = self.limit.clone();
        loop {
            let count = *active.borrow_and_update();
            let cap = *limit.borrow_and_update();
            if cap == 0 || count < cap {
                let until = *self.cooldown.lock().unwrap();
                if until.is_some_and(|until| until <= tokio::time::Instant::now()) {
                    self.active.send_modify(|active| *active += 1);
                    return ModelPermit(&self.active);
                }
                wait_until(until).await;
            } else {
                tokio::select! {
                    _ = active.changed() => {},
                    _ = async {
                        if limit.changed().await.is_err() {
                            std::future::pending::<()>().await;
                        }
                    } => {},
                }
            }
        }
    }

    fn defer(&self, delay: Duration) {
        let mut until = self.cooldown.lock().unwrap();
        *until = until
            .zip(tokio::time::Instant::now().checked_add(delay))
            .map(|(old, next)| old.max(next));
    }
}

/// User-adjustable retry policy. Provider Retry-After always remains a minimum.
#[derive(Clone, Copy)]
pub struct ModelRetryConfig {
    pub max_attempts: Option<u32>,
    pub backoff: Duration,
    pub max_backoff: Option<Duration>,
}

impl Default for ModelRetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: None,
            backoff: Duration::from_millis(200),
            max_backoff: None,
        }
    }
}

/// Wraps a model with optional retry limits. A host can share admission across wrappers
/// and supply provider-specific retry timing without coupling extensions to HTTP.
pub struct RetryModel<M: Model> {
    inner: M,
    max_attempts: Option<u32>,
    backoff: Duration,
    on_retry: Option<ModelRetryNotice>,
    config: Option<Arc<dyn Fn() -> ModelRetryConfig + Send + Sync>>,
    gate: Option<ModelGate>,
    retry_delay: fn(&ModelError) -> Option<Duration>,
}

impl<M: Model> RetryModel<M> {
    /// None retries until success, a permanent error, or caller cancellation/deadline.
    pub fn new(inner: M, max_attempts: impl Into<Option<u32>>) -> Self {
        Self {
            inner,
            max_attempts: max_attempts.into().map(|attempts| attempts.max(1)),
            backoff: ModelRetryConfig::default().backoff,
            on_retry: None,
            config: None,
            gate: None,
            retry_delay: |error| match error {
                ModelError::Request(_)
                | ModelError::OutputLimit { .. }
                | ModelError::IncompleteResponse { .. } => Some(Duration::ZERO),
                _ => None,
            },
        }
    }

    /// Base exponential backoff. Provider timing remains a minimum wait.
    pub fn backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Read host settings at each failure, so existing wrappers see live edits.
    pub fn config(mut self, config: impl Fn() -> ModelRetryConfig + Send + Sync + 'static) -> Self {
        self.config = Some(Arc::new(config));
        self
    }

    pub fn gate(mut self, gate: ModelGate) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Return a minimum wait for retryable errors, or `None` to stop.
    pub fn retry_delay(mut self, policy: fn(&ModelError) -> Option<Duration>) -> Self {
        self.retry_delay = policy;
        self
    }

    /// Observe the upcoming attempt (starting at two) without coupling to UI.
    pub fn on_retry(
        mut self,
        callback: impl Fn(u32, Option<u32>, &ModelError) + Send + Sync + 'static,
    ) -> Self {
        self.on_retry = Some(Arc::new(callback));
        self
    }

    async fn run(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: Option<&dyn DeltaSink>,
    ) -> Result<ModelResponse, ModelError> {
        let mut attempt = 1u32;
        let mut base = self.backoff;
        let mut ceiling = base;
        loop {
            let permit = match &self.gate {
                Some(gate) => Some(gate.acquire().await),
                None => None,
            };
            let tracked = TrackedSink {
                sink,
                emitted: std::sync::atomic::AtomicBool::new(false),
            };
            let result = if sink.is_some() {
                self.inner
                    .generate_streaming(context, tools, &tracked)
                    .await
            } else {
                self.inner.generate(context, tools).await
            };
            let error = match result {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };
            let Some(minimum) = (self.retry_delay)(&error) else {
                return Err(error);
            };
            let config = self.config.as_ref().map_or(
                ModelRetryConfig {
                    max_attempts: self.max_attempts,
                    backoff: self.backoff,
                    max_backoff: None,
                },
                |config| config(),
            );
            if base != config.backoff {
                base = config.backoff;
                ceiling = base;
            }
            let bounded = config.max_backoff.map_or(ceiling, |max| ceiling.min(max));
            let delay = minimum.max(bounded.mul_f64(fastrand::f64()));
            // Publish before releasing the permit, including on the final failure.
            if let Some(gate) = &self.gate {
                gate.defer(delay);
            }
            drop(permit);
            // Deltas have already reached the caller; replay would corrupt its
            // provisional text/tool input. Let the caller handle that failure.
            if config.max_attempts.is_some_and(|max| attempt >= max)
                || tracked.emitted.load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(error);
            }
            if let Some(notice) = &self.on_retry {
                notice(attempt.saturating_add(1), config.max_attempts, &error);
            }
            wait_until(tokio::time::Instant::now().checked_add(delay)).await;
            attempt = attempt.saturating_add(1);
            ceiling = ceiling.saturating_mul(2);
        }
    }
}

// An unrepresentable future instant must remain cancellable, not panic or
// silently turn an overflowing backoff into an immediate retry.
async fn wait_until(until: Option<tokio::time::Instant>) {
    match until {
        Some(until) => tokio::time::sleep_until(until).await,
        None => std::future::pending().await,
    }
}

struct TrackedSink<'a> {
    sink: Option<&'a dyn DeltaSink>,
    emitted: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl DeltaSink for TrackedSink<'_> {
    async fn emit(&self, delta: orca_harness_core::ModelDelta) {
        self.emitted
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(sink) = self.sink {
            sink.emit(delta).await;
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
        self.run(context, tools, None).await
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        self.run(context, tools, Some(sink)).await
    }
}
