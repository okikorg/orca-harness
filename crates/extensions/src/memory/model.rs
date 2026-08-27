use async_trait::async_trait;

use orca_harness_core::{Context, DeltaSink, Model, ModelError, ModelResponse, ToolSchema};

use super::MemoryExtension;

/// Adds recalled memory to a temporary model-only context.
pub struct MemoryModel<M> {
    inner: M,
    memory: MemoryExtension,
}

impl<M> MemoryModel<M> {
    pub fn new(inner: M, memory: MemoryExtension) -> Self {
        Self { inner, memory }
    }

    fn context(&self, context: &Context) -> Result<Option<Context>, ModelError> {
        self.memory
            .prepare_context(context)
            .map_err(|error| ModelError::Request(format!("local memory retrieval failed: {error}")))
    }
}

#[async_trait]
impl<M: Model> Model for MemoryModel<M> {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        let model_context = self.context(context)?;
        self.inner
            .generate(model_context.as_ref().unwrap_or(context), tools)
            .await
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        let model_context = self.context(context)?;
        self.inner
            .generate_streaming(model_context.as_ref().unwrap_or(context), tools, sink)
            .await
    }
}
