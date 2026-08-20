//! The Model boundary. The Loop contains no provider-specific logic;
//! adapters (OpenAI-compatible, Anthropic, local inference, ...) live in
//! separate crates behind this trait.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::context::Context;
use crate::error::ModelError;
use crate::tool::{ToolCall, ToolSchema};

/// Token accounting for one model invocation, self-reported by the
/// adapter. The kernel never inspects it; extensions (metering, billing,
/// event streams) consume it via `after_model`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_create_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_create_tokens += other.cache_create_tokens;
    }
}

/// An incremental fragment emitted while the model is still generating.
/// Deltas are presentation-only: the [`ModelResponse`] returned by
/// [`Model::generate_streaming`] stays the source of truth the loop acts
/// on, and the kernel never accumulates deltas itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelDelta {
    /// A fragment of assistant text.
    Text { text: String },
    /// A fragment of the model's reasoning/thinking channel. Ephemeral:
    /// reasoning never enters the Context.
    Reasoning { text: String },
}

/// Receives deltas during a streaming generation. Implemented for plain
/// `Fn(ModelDelta)` closures.
#[async_trait]
pub trait DeltaSink: Send + Sync {
    async fn emit(&self, delta: ModelDelta);
}

#[async_trait]
impl<F> DeltaSink for F
where
    F: Fn(ModelDelta) + Send + Sync,
{
    async fn emit(&self, delta: ModelDelta) {
        self(delta)
    }
}

#[derive(Debug, Clone)]
pub enum ModelResponse {
    /// A final answer: the run terminates.
    Final { text: String, usage: Option<Usage> },
    /// One batch of tool calls to execute before the next model step.
    ToolCalls {
        /// Assistant text accompanying the calls, if any.
        content: Option<String>,
        calls: Vec<ToolCall>,
        usage: Option<Usage>,
    },
}

impl ModelResponse {
    /// A final answer with no usage attached.
    pub fn final_text(text: impl Into<String>) -> Self {
        Self::Final {
            text: text.into(),
            usage: None,
        }
    }

    /// A tool-call batch with no accompanying text or usage.
    pub fn tool_calls(calls: Vec<ToolCall>) -> Self {
        Self::ToolCalls {
            content: None,
            calls,
            usage: None,
        }
    }

    pub fn usage(&self) -> Option<&Usage> {
        match self {
            Self::Final { usage, .. } | Self::ToolCalls { usage, .. } => usage.as_ref(),
        }
    }
}

#[async_trait]
pub trait Model: Send + Sync {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError>;

    /// Like [`generate`](Model::generate), but emits incremental
    /// [`ModelDelta`]s to `sink` while the response is produced. The
    /// returned response is authoritative. Defaults to plain `generate`
    /// with no deltas, so non-streaming adapters need no changes; the loop
    /// only calls this when at least one extension subscribes to deltas.
    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        _sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        self.generate(context, tools).await
    }
}

// Shared handles are models too, so hosts can keep a reference to the
// model (e.g. a test double) while the Agent owns another.
#[async_trait]
impl<M: Model + ?Sized> Model for std::sync::Arc<M> {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        (**self).generate(context, tools).await
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        (**self).generate_streaming(context, tools, sink).await
    }
}
