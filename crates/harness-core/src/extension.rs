//! Extensions: the single general-purpose extensibility mechanism.
//! They surround execution without replacing the kernel.
//!
//! At Agent construction, subscriptions are compiled into event-specific
//! arrays. An event with no subscribers costs one empty-slice check — unused
//! extensibility approaches zero cost.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::context::Context;
use crate::error::{ExtensionError, HarnessError, ToolError};
use crate::model::{ModelDelta, ModelResponse};
use crate::tool::{Tool, ToolCall, ToolContext, ToolResult};

/// Which lifecycle events an extension subscribes to. Narrow this to keep
/// unsubscribed events on the kernel's direct path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subscriptions {
    pub on_agent_start: bool,
    pub before_model: bool,
    pub model_delta: bool,
    pub after_model: bool,
    pub before_tool: bool,
    pub around_tool: bool,
    pub after_tool: bool,
    pub tool_finished: bool,
    pub tool_result: bool,
    pub on_error: bool,
    pub on_agent_end: bool,
}

impl Subscriptions {
    pub const ALL: Self = Self {
        on_agent_start: true,
        before_model: true,
        model_delta: true,
        after_model: true,
        before_tool: true,
        around_tool: true,
        after_tool: true,
        tool_finished: true,
        tool_result: true,
        on_error: true,
        on_agent_end: true,
    };

    pub const NONE: Self = Self {
        on_agent_start: false,
        before_model: false,
        model_delta: false,
        after_model: false,
        before_tool: false,
        around_tool: false,
        after_tool: false,
        tool_finished: false,
        tool_result: false,
        on_error: false,
        on_agent_end: false,
    };

    pub fn none() -> Self {
        Self::NONE
    }

    pub fn on_agent_start(mut self) -> Self {
        self.on_agent_start = true;
        self
    }
    pub fn before_model(mut self) -> Self {
        self.before_model = true;
        self
    }
    pub fn model_delta(mut self) -> Self {
        self.model_delta = true;
        self
    }
    pub fn after_model(mut self) -> Self {
        self.after_model = true;
        self
    }
    pub fn before_tool(mut self) -> Self {
        self.before_tool = true;
        self
    }
    pub fn around_tool(mut self) -> Self {
        self.around_tool = true;
        self
    }
    pub fn after_tool(mut self) -> Self {
        self.after_tool = true;
        self
    }
    pub fn tool_finished(mut self) -> Self {
        self.tool_finished = true;
        self
    }
    pub fn tool_result(mut self) -> Self {
        self.tool_result = true;
        self
    }
    pub fn on_error(mut self) -> Self {
        self.on_error = true;
        self
    }
    pub fn on_agent_end(mut self) -> Self {
        self.on_agent_end = true;
        self
    }
}

/// Outcome of `before_tool`.
#[derive(Debug, Clone)]
pub enum ToolDecision {
    Continue,
    /// Replace the call's arguments before execution.
    Rewrite(Value),
    /// Block the call. The model sees an error-flagged result carrying
    /// `reason`; execution and `around_tool`/`after_tool` are skipped.
    Deny {
        reason: String,
    },
}

/// Continuation handed to `around_tool`: calls the rest of the wrapper
/// chain and, at the end, the tool itself. Wrappers may time, retry,
/// or replace execution without becoming the Dispatcher. `Next` is
/// `Copy`, so a wrapper can invoke the continuation more than once
/// (retries) by keeping a copy before calling `run`.
#[derive(Clone, Copy)]
pub struct Next<'a> {
    pub(crate) chain: &'a [Arc<dyn Extension>],
    pub(crate) call: &'a ToolCall,
    pub(crate) tool: &'a dyn Tool,
    pub(crate) ctx: &'a ToolContext,
}

impl<'a> Next<'a> {
    pub fn run(
        self,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            match self.chain.split_first() {
                Some((head, rest)) => {
                    let next = Next {
                        chain: rest,
                        call: self.call,
                        tool: self.tool,
                        ctx: self.ctx,
                    };
                    head.around_tool(self.call, input, self.ctx, next).await
                }
                None => self.tool.call(input, self.ctx).await,
            }
        })
    }
}

#[async_trait]
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;

    /// Defaults to [`Subscriptions::ALL`] so a naive extension never misses
    /// events. Override and narrow to what you actually use.
    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::ALL
    }

    async fn on_agent_start(&self, _context: &mut Context) -> Result<(), ExtensionError> {
        Ok(())
    }

    async fn before_model(&self, _context: &mut Context) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// Immutable observation of a streaming fragment (rendering,
    /// telemetry). Only fires when the model streams; the authoritative
    /// response still arrives via `after_model`.
    async fn on_model_delta(&self, _delta: &ModelDelta) {}

    async fn after_model(
        &self,
        _context: &mut Context,
        _response: &ModelResponse,
    ) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// Policy, validation, approval, argument rewriting. Runs
    /// deterministically in call order before any execution starts.
    async fn before_tool(&self, _call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        Ok(ToolDecision::Continue)
    }

    /// Timeout, retry, metrics, tracing, sandbox routing. Must invoke
    /// `next.run(input)` (possibly more than once) or produce a result
    /// itself.
    async fn around_tool<'a>(
        &self,
        _call: &ToolCall,
        input: Value,
        _ctx: &ToolContext,
        next: Next<'a>,
    ) -> Result<Value, ToolError> {
        next.run(input).await
    }

    /// Truncation, redaction, normalization. Runs deterministically in
    /// call order after all results are collected.
    async fn after_tool(
        &self,
        _call: &ToolCall,
        result: ToolResult,
    ) -> Result<ToolResult, ExtensionError> {
        Ok(result)
    }

    /// Live observation that execution has ended for one call. Unlike
    /// `tool_result`, this fires from the executing task in completion order,
    /// before batch-wide result collection and result-transforming hooks.
    /// It intentionally exposes only identity and error status so unredacted
    /// tool output cannot escape through telemetry. Denied calls fire this
    /// hook during preflight because they never enter an execution task.
    async fn tool_finished(&self, _call_id: &str, _tool_name: &str, _is_error: bool) {}

    /// Immutable observation of the final result (telemetry/audit).
    /// Denied calls are observed here too.
    async fn tool_result(&self, _result: &ToolResult) {}

    async fn on_error(&self, _error: &HarnessError) {}

    async fn on_agent_end(&self, _context: &Context) {}
}

/// Subscriptions compiled into event-specific arrays at registration time.
#[derive(Default, Clone)]
pub struct ExtensionRegistry {
    on_agent_start: Vec<Arc<dyn Extension>>,
    before_model: Vec<Arc<dyn Extension>>,
    model_delta: Vec<Arc<dyn Extension>>,
    after_model: Vec<Arc<dyn Extension>>,
    before_tool: Vec<Arc<dyn Extension>>,
    around_tool: Vec<Arc<dyn Extension>>,
    after_tool: Vec<Arc<dyn Extension>>,
    tool_finished: Vec<Arc<dyn Extension>>,
    tool_result: Vec<Arc<dyn Extension>>,
    on_error: Vec<Arc<dyn Extension>>,
    on_agent_end: Vec<Arc<dyn Extension>>,
}

impl ExtensionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, extension: Arc<dyn Extension>) {
        let subs = extension.subscriptions();
        if subs.on_agent_start {
            self.on_agent_start.push(extension.clone());
        }
        if subs.before_model {
            self.before_model.push(extension.clone());
        }
        if subs.model_delta {
            self.model_delta.push(extension.clone());
        }
        if subs.after_model {
            self.after_model.push(extension.clone());
        }
        if subs.before_tool {
            self.before_tool.push(extension.clone());
        }
        if subs.around_tool {
            self.around_tool.push(extension.clone());
        }
        if subs.after_tool {
            self.after_tool.push(extension.clone());
        }
        if subs.tool_finished {
            self.tool_finished.push(extension.clone());
        }
        if subs.tool_result {
            self.tool_result.push(extension.clone());
        }
        if subs.on_error {
            self.on_error.push(extension.clone());
        }
        if subs.on_agent_end {
            self.on_agent_end.push(extension);
        }
    }

    pub(crate) fn around_tool_chain(&self) -> &[Arc<dyn Extension>] {
        &self.around_tool
    }

    pub(crate) fn before_tool_subscribers(&self) -> &[Arc<dyn Extension>] {
        &self.before_tool
    }

    pub(crate) fn after_tool_subscribers(&self) -> &[Arc<dyn Extension>] {
        &self.after_tool
    }

    pub(crate) fn tool_finished_subscribers(&self) -> &[Arc<dyn Extension>] {
        &self.tool_finished
    }

    pub(crate) fn tool_result_subscribers(&self) -> &[Arc<dyn Extension>] {
        &self.tool_result
    }

    pub(crate) async fn run_on_agent_start(
        &self,
        context: &mut Context,
    ) -> Result<(), HarnessError> {
        for ext in &self.on_agent_start {
            ext.on_agent_start(context).await?;
        }
        Ok(())
    }

    pub(crate) fn model_delta_subscribers(&self) -> &[Arc<dyn Extension>] {
        &self.model_delta
    }

    pub(crate) async fn run_before_model(&self, context: &mut Context) -> Result<(), HarnessError> {
        for ext in &self.before_model {
            ext.before_model(context).await?;
        }
        Ok(())
    }

    pub(crate) async fn run_after_model(
        &self,
        context: &mut Context,
        response: &ModelResponse,
    ) -> Result<(), HarnessError> {
        for ext in &self.after_model {
            ext.after_model(context, response).await?;
        }
        Ok(())
    }

    pub(crate) async fn run_on_error(&self, error: &HarnessError) {
        for ext in &self.on_error {
            ext.on_error(error).await;
        }
    }

    pub(crate) async fn run_on_agent_end(&self, context: &Context) {
        for ext in &self.on_agent_end {
            ext.on_agent_end(context).await;
        }
    }
}
