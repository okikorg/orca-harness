/// Minimal usage accumulator for the inner agent. Local to this module:
/// pulling in the extensions crate for one hook would invert the crate
/// layering.
#[derive(Clone, Default)]
struct Meter {
    usage: Arc<StdMutex<Usage>>,
    steps: Arc<AtomicU32>,
    tool_calls: Arc<AtomicU32>,
    timing: Arc<StdMutex<TimingMeter>>,
    timing_entry_limit: usize,
}

#[derive(Default)]
struct TimingMeter {
    model_started: Option<std::time::Instant>,
    model_call_elapsed_ms: Vec<u128>,
    model_cumulative_ms: u128,
    tool_call_elapsed: Vec<ToolCallTiming>,
    tool_cumulative_ms: u128,
}

impl Meter {
    fn new(timing_entry_limit: usize) -> Self {
        Self {
            timing_entry_limit,
            ..Self::default()
        }
    }

    fn total(&self) -> Usage {
        *self.usage.lock().unwrap()
    }

    fn steps(&self) -> u32 {
        self.steps.load(Ordering::Relaxed)
    }

    fn tool_calls(&self) -> u32 {
        self.tool_calls.load(Ordering::Relaxed)
    }

    fn timing(&self) -> WorkerTiming {
        let timing = self.timing.lock().unwrap();
        WorkerTiming {
            model_call_elapsed_ms: timing.model_call_elapsed_ms.clone(),
            model_cumulative_ms: timing.model_cumulative_ms,
            tool_call_elapsed: timing.tool_call_elapsed.clone(),
            tool_cumulative_ms: timing.tool_cumulative_ms,
        }
    }
}

#[async_trait]
impl Extension for Meter {
    fn name(&self) -> &str {
        "subagent_usage"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none()
            .before_model()
            .after_model()
            .around_tool()
    }

    async fn before_model(&self, _context: &mut Context) -> Result<(), ExtensionError> {
        self.steps.fetch_add(1, Ordering::Relaxed);
        self.timing.lock().unwrap().model_started = Some(std::time::Instant::now());
        Ok(())
    }

    async fn after_model(
        &self,
        _context: &mut Context,
        response: &ModelResponse,
    ) -> Result<(), ExtensionError> {
        let mut timing = self.timing.lock().unwrap();
        if let Some(started) = timing.model_started.take() {
            let elapsed_ms = started.elapsed().as_millis();
            timing.model_cumulative_ms += elapsed_ms;
            if timing.model_call_elapsed_ms.len() < self.timing_entry_limit {
                timing.model_call_elapsed_ms.push(elapsed_ms);
            }
        }
        drop(timing);
        if let Some(usage) = response.usage() {
            self.usage.lock().unwrap().add(usage);
        }
        if let ModelResponse::ToolCalls { calls, .. } = response {
            self.tool_calls
                .fetch_add(calls.len() as u32, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn around_tool<'a>(
        &self,
        call: &ToolCall,
        input: Value,
        _ctx: &ToolContext,
        next: Next<'a>,
    ) -> Result<Value, ToolError> {
        let started = std::time::Instant::now();
        let result = next.run(input).await;
        let elapsed_ms = started.elapsed().as_millis();
        let mut timing = self.timing.lock().unwrap();
        timing.tool_cumulative_ms += elapsed_ms;
        if timing.tool_call_elapsed.len() < self.timing_entry_limit {
            timing.tool_call_elapsed.push(ToolCallTiming {
                name: call.name.clone(),
                elapsed_ms,
            });
        }
        result
    }
}

/// Tool-call retry for *inner* agents. Deliberately a local copy of the
/// extensions crate's `ToolRetry` semantics (crate-layering:
/// `orca-harness-tools` must not depend on `orca-harness-extensions`):
/// re-invoke a failing call up to `max_attempts` times, and treat an
/// `Ok` result the [`OkFailureRule`] rejects as a failed attempt too,
/// while an `Err` the [`ErrorRetryRule`] refuses is returned at once.
#[derive(Clone)]
struct SubagentRetry {
    max_attempts: u32,
    backoff: std::time::Duration,
    /// Data-failure rule for the inherited [`RetryPolicy`].
    ok_failure: Option<OkFailureRule>,
    /// Error restriction; `None` retries every `Err`.
    retry_error: Option<ErrorRetryRule>,
}

impl SubagentRetry {
    fn new(
        policy: RetryPolicy,
        ok_failure: Option<OkFailureRule>,
        retry_error: Option<ErrorRetryRule>,
    ) -> Self {
        Self {
            max_attempts: policy.0,
            backoff: policy.1,
            ok_failure,
            retry_error,
        }
    }
}

#[async_trait]
impl Extension for SubagentRetry {
    fn name(&self) -> &str {
        "subagent-tool-retry"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().around_tool()
    }

    async fn around_tool<'a>(
        &self,
        call: &ToolCall,
        input: Value,
        _ctx: &ToolContext,
        next: Next<'a>,
    ) -> Result<Value, ToolError> {
        let mut last_err = None;
        let mut last_value = None;
        for attempt in 1..=self.max_attempts {
            match next.run(input.clone()).await {
                Ok(value) => {
                    if self
                        .ok_failure
                        .as_ref()
                        .is_some_and(|rule| rule(call, &value))
                    {
                        last_value = Some(value);
                        if attempt == self.max_attempts {
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
                        tokio::time::sleep(self.backoff).await;
                    }
                }
            }
        }
        match last_value {
            Some(value) => Ok(value),
            None => {
                Err(last_err.unwrap_or_else(|| ToolError::msg("subagent retry: no attempts made")))
            }
        }
    }
}
