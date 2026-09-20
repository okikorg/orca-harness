//! Parent-only, request-local instructions; never relocate durable messages.
use async_trait::async_trait;
use orca_harness_core::{
    Context, DeltaSink, Message, Model, ModelError, ModelResponse, ToolSchema,
};

use super::{Mode, ModeHandle};

const ORCHESTRATE_BRIEFING: &str = "Orchestrate mode is on for the parent: prioritize orchestration and delegation. Investigate, break substantial work into bounded tasks for subagent or workflow, and synthesize results. Small/basic code edits using write_file, edit_file, multi_edit, or apply_patch are permitted; delegate significant implementation and testing. This is a behavioral distinction, not a line-count limit. Shell, process, compute, MCP, and other non-allowlisted tools are blocked for the parent. Workers keep their existing tools and approval behavior.";

pub(crate) struct OrchestrateModel<M> {
    inner: M,
    mode: ModeHandle,
}

impl<M> OrchestrateModel<M> {
    pub(crate) fn new(inner: M, mode: ModeHandle) -> Self {
        Self { inner, mode }
    }

    fn context(&self, context: &Context) -> Option<Context> {
        if self.mode.get() != Mode::Orchestrate {
            return None;
        }
        let mut request = Context::new();
        match context.messages().split_first() {
            Some((Message::System { content }, rest)) => {
                request.push_system(format!("{content}\n\n{ORCHESTRATE_BRIEFING}"));
                for message in rest {
                    request.push(message.clone());
                }
            }
            _ => {
                request.push_system(ORCHESTRATE_BRIEFING);
                for message in context.messages() {
                    request.push(message.clone());
                }
            }
        }
        Some(request)
    }
}

#[async_trait]
impl<M: Model> Model for OrchestrateModel<M> {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        let request = self.context(context);
        self.inner
            .generate(request.as_ref().unwrap_or(context), tools)
            .await
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        let request = self.context(context);
        self.inner
            .generate_streaming(request.as_ref().unwrap_or(context), tools, sink)
            .await
    }
}
