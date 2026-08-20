//! Test doubles: a scripted fake model and small instrumented tools.
//! Used by the harness's own tests, benchmarks, and examples; exported so
//! hosts can drive the kernel deterministically in their tests too.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use crate::context::Context;
use crate::error::ModelError;
use crate::model::{DeltaSink, Model, ModelDelta, ModelResponse};
use crate::tool::{ToolCall, ToolSchema};

/// A fake LLM that replays a fixed script of responses, one per
/// `generate` call. Running past the end of the script is a test bug and
/// reported as a model error.
pub struct ScriptedModel {
    script: Mutex<VecDeque<ModelResponse>>,
    /// Delta batches replayed by `generate_streaming`, one per call.
    deltas: Mutex<VecDeque<Vec<ModelDelta>>>,
    /// Number of `generate` calls served.
    calls: AtomicUsize,
    /// Number of `generate_streaming` calls served.
    streaming_calls: AtomicUsize,
    /// Context snapshot seen by each `generate` call, for assertions.
    observed: Mutex<Vec<Context>>,
}

impl ScriptedModel {
    pub fn new(script: Vec<ModelResponse>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            deltas: Mutex::new(VecDeque::new()),
            calls: AtomicUsize::new(0),
            streaming_calls: AtomicUsize::new(0),
            observed: Mutex::new(Vec::new()),
        }
    }

    /// Script the delta batches `generate_streaming` emits: one batch per
    /// model call, emitted before the scripted response is returned.
    pub fn with_deltas(self, deltas: Vec<Vec<ModelDelta>>) -> Self {
        *self.deltas.lock().unwrap() = deltas.into();
        self
    }

    /// Script a single batch of tool calls followed by a final answer.
    pub fn tool_round(calls: Vec<ToolCall>, final_answer: impl Into<String>) -> Self {
        Self::new(vec![
            ModelResponse::ToolCalls {
                content: None,
                calls,
                usage: None,
            },
            ModelResponse::final_text(final_answer.into()),
        ])
    }

    pub fn generate_calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn streaming_calls(&self) -> usize {
        self.streaming_calls.load(Ordering::SeqCst)
    }

    fn next_response(&self, context: &Context) -> Result<ModelResponse, ModelError> {
        self.observed.lock().unwrap().push(context.clone());
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::InvalidResponse("scripted model ran out of script".into()))
    }

    pub fn observed_contexts(&self) -> Vec<Context> {
        self.observed.lock().unwrap().clone()
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn generate(
        &self,
        context: &Context,
        _tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.next_response(context)
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        _tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        self.streaming_calls.fetch_add(1, Ordering::SeqCst);
        let batch = self.deltas.lock().unwrap().pop_front().unwrap_or_default();
        for delta in batch {
            sink.emit(delta).await;
        }
        self.next_response(context)
    }
}

/// Convenience constructor for scripted tool calls.
pub fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

/// Tracks how many tool executions overlap in time, and the maximum
/// overlap observed. The backbone of the concurrency assertions.
#[derive(Default)]
pub struct ConcurrencyProbe {
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    /// Order in which executions started / finished (call ids).
    started: Mutex<Vec<String>>,
    finished: Mutex<Vec<String>>,
}

impl ConcurrencyProbe {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Mark an execution as started; returns a guard that marks it
    /// finished on drop.
    pub fn enter(self: &Arc<Self>, id: &str) -> ProbeGuard {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        self.started.lock().unwrap().push(id.to_string());
        ProbeGuard {
            probe: self.clone(),
            id: id.to_string(),
        }
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }

    pub fn started(&self) -> Vec<String> {
        self.started.lock().unwrap().clone()
    }

    pub fn finished(&self) -> Vec<String> {
        self.finished.lock().unwrap().clone()
    }
}

pub struct ProbeGuard {
    probe: Arc<ConcurrencyProbe>,
    id: String,
}

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        self.probe.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.probe.finished.lock().unwrap().push(self.id.clone());
    }
}
