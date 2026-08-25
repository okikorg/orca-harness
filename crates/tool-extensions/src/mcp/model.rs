//! Model adapter that hides unselected remote MCP schemas.
//!
//! The core loop keeps its immutable schema snapshot. The adapter applies
//! current catalog visibility immediately before each provider request, so a
//! successful selection affects the next model turn without changing core.

use async_trait::async_trait;

use orca_harness_core::{Context, DeltaSink, Model, ModelError, ModelResponse, ToolSchema};

use super::McpCatalog;

pub struct McpModel<M> {
    inner: M,
    catalog: McpCatalog,
}

impl<M> McpModel<M> {
    pub fn new(inner: M, catalog: McpCatalog) -> Self {
        Self { inner, catalog }
    }

    fn visible(&self, tools: &[ToolSchema]) -> Vec<ToolSchema> {
        tools
            .iter()
            .filter(|schema| self.catalog.schema_visible(&schema.name))
            .cloned()
            .collect()
    }
}

#[async_trait]
impl<M: Model> Model for McpModel<M> {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.inner.generate(context, &self.visible(tools)).await
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        self.inner
            .generate_streaming(context, &self.visible(tools), sink)
            .await
    }
}
