//! Usage metering extension: accumulates token usage across all model
//! steps of a run. The platform's only token-accounting channel is
//! self-reported usage, so a harness that reports nothing runs free — this
//! is how the harness reports.
//!
//! The running total lives behind a shared handle the host holds, so it
//! can read the final figure after the run returns (the extension is owned
//! by the Agent and not otherwise reachable).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use orca_harness_core::{
    Context, Extension, ExtensionError, HarnessError, ModelError, ModelResponse, Subscriptions,
    Usage,
};

/// A cloneable handle to a run's cumulative token usage. Cheap to clone;
/// all clones observe the same counters.
#[derive(Clone, Default)]
pub struct UsageHandle {
    input: Arc<AtomicU64>,
    output: Arc<AtomicU64>,
    reasoning: Arc<AtomicU64>,
    reasoning_reported: Arc<AtomicBool>,
    cache_read: Arc<AtomicU64>,
    cache_create: Arc<AtomicU64>,
    /// Reported cost, summed by `Usage::add`; there is no atomic float.
    cost: Arc<Mutex<Usage>>,
    steps: Arc<AtomicU64>,
}

impl UsageHandle {
    pub fn new() -> Self {
        Self::default()
    }

    fn add(&self, usage: &Usage) {
        self.input.fetch_add(usage.input_tokens, Ordering::Relaxed);
        self.output
            .fetch_add(usage.output_tokens, Ordering::Relaxed);
        self.cache_read
            .fetch_add(usage.cache_read_tokens, Ordering::Relaxed);
        self.cache_create
            .fetch_add(usage.cache_create_tokens, Ordering::Relaxed);
        if let Some(tokens) = usage.reasoning_tokens {
            self.reasoning.fetch_add(tokens, Ordering::Relaxed);
            self.reasoning_reported.store(true, Ordering::Relaxed);
        }
        self.cost.lock().unwrap().add(&Usage {
            cost: usage.cost,
            cost_details: usage.cost_details,
            ..Usage::default()
        });
        self.steps.fetch_add(1, Ordering::Relaxed);
    }

    /// The cumulative usage so far.
    pub fn total(&self) -> Usage {
        let cost = *self.cost.lock().unwrap();
        Usage {
            input_tokens: self.input.load(Ordering::Relaxed),
            output_tokens: self.output.load(Ordering::Relaxed),
            cache_read_tokens: self.cache_read.load(Ordering::Relaxed),
            cache_create_tokens: self.cache_create.load(Ordering::Relaxed),
            reasoning_tokens: self
                .reasoning_reported
                .load(Ordering::Relaxed)
                .then(|| self.reasoning.load(Ordering::Relaxed)),
            cost: cost.cost,
            cost_details: cost.cost_details,
        }
    }

    /// Number of model steps that reported usage.
    pub fn metered_steps(&self) -> u64 {
        self.steps.load(Ordering::Relaxed)
    }
}

/// Usage the provider reported before the call failed. These calls are
/// billed, so meters must count them like a successful step.
pub(crate) fn failed_call_usage(error: &ModelError) -> Option<&Usage> {
    match error {
        ModelError::OutputLimit { usage, .. }
        | ModelError::ContentFiltered { usage, .. }
        | ModelError::IncompleteResponse { usage, .. }
        | ModelError::MalformedToolArguments { usage, .. } => usage.as_ref(),
        ModelError::Authentication(_) | ModelError::Request(_) | ModelError::InvalidResponse(_) => {
            None
        }
    }
}

/// Aggregates per-step usage into a [`UsageHandle`].
pub struct UsageMeter {
    handle: UsageHandle,
}

impl UsageMeter {
    /// Create a meter and the handle the host reads after the run.
    pub fn new() -> (Self, UsageHandle) {
        let handle = UsageHandle::new();
        (
            Self {
                handle: handle.clone(),
            },
            handle,
        )
    }
}

#[async_trait]
impl Extension for UsageMeter {
    fn name(&self) -> &str {
        "usage-meter"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().after_model().on_error()
    }

    async fn after_model(
        &self,
        _context: &mut Context,
        response: &ModelResponse,
    ) -> Result<(), ExtensionError> {
        if let Some(usage) = response.usage() {
            self.handle.add(usage);
        }
        Ok(())
    }

    /// A failed model call ends the run, so this fires at most once per call.
    async fn on_error(&self, error: &HarnessError) {
        if let HarnessError::Model(error) = error {
            if let Some(usage) = failed_call_usage(error) {
                self.handle.add(usage);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_breakdown_is_optional_and_never_added_to_output() {
        let handle = UsageHandle::new();
        assert_eq!(handle.total().reasoning_tokens, None);
        let mut total = Usage::default();
        for reasoning_tokens in [Some(20), None, Some(30)] {
            let usage = Usage {
                output_tokens: 50,
                reasoning_tokens,
                ..Usage::default()
            };
            handle.add(&usage);
            total.add(&usage);
        }
        assert_eq!(handle.total(), total);
        assert_eq!(total.reasoning_tokens, Some(50));
        assert_eq!(total.context_tokens(), 150);
        let legacy = serde_json::json!({"inputTokens": 0, "outputTokens": 50, "cacheReadTokens": 0, "cacheCreateTokens": 0});
        assert_eq!(
            serde_json::from_value::<Usage>(legacy)
                .unwrap()
                .reasoning_tokens,
            None
        );
    }

    #[test]
    fn reported_cost_is_summed_and_absent_cost_stays_absent() {
        use orca_harness_core::CostDetails;

        let handle = UsageHandle::new();
        handle.add(&Usage {
            output_tokens: 7,
            ..Usage::default()
        });
        assert_eq!(handle.total().cost, None);
        assert_eq!(handle.total().cost_details, None);

        let mut total = Usage::default();
        for cost in [Some(0.25), None, Some(0.5)] {
            let usage = Usage {
                cost,
                cost_details: cost.map(|cost| CostDetails {
                    upstream_inference_cost: Some(cost),
                }),
                ..Usage::default()
            };
            handle.add(&usage);
            total.add(&usage);
        }
        assert_eq!(handle.total().cost, Some(0.75));
        assert_eq!(
            handle.total().cost_details,
            Some(CostDetails {
                upstream_inference_cost: Some(0.75)
            })
        );
        assert_eq!(total.cost, handle.total().cost);

        let legacy = serde_json::json!({"inputTokens": 1, "outputTokens": 2, "cacheReadTokens": 0, "cacheCreateTokens": 0});
        let read: Usage = serde_json::from_value(legacy).unwrap();
        assert_eq!((read.cost, read.cost_details), (None, None));
        let written = serde_json::to_value(read).unwrap();
        assert!(written.get("cost").is_none() && written.get("costDetails").is_none());
    }
}
