//! Agent: configuration plus an entry point into the loop.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::agent_loop::{self, LoopEnv};
use crate::context::Context;
use crate::dispatcher::Dispatcher;
use crate::error::HarnessError;
use crate::extension::{Extension, ExtensionRegistry};
use crate::limits::Limits;
use crate::model::Model;
use crate::tool::{Tool, ToolRegistry};

pub struct Agent<M: Model> {
    model: M,
    tools: ToolRegistry,
    extensions: ExtensionRegistry,
    limits: Limits,
    system_prompt: Option<String>,
    dispatcher: Dispatcher,
}

impl<M: Model> Agent<M> {
    pub fn new(model: M) -> Self {
        Self {
            model,
            tools: ToolRegistry::new(),
            extensions: ExtensionRegistry::new(),
            limits: Limits::default(),
            system_prompt: None,
            dispatcher: Dispatcher::new(),
        }
    }

    pub fn tool(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.register(Arc::new(tool));
        self
    }

    pub fn tool_arc(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.register(tool);
        self
    }

    pub fn extension(mut self, extension: impl Extension + 'static) -> Self {
        self.extensions.register(Arc::new(extension));
        self
    }

    pub fn extension_arc(mut self, extension: Arc<dyn Extension>) -> Self {
        self.extensions.register(extension);
        self
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Run to completion from a single user prompt.
    pub async fn run(&self, prompt: &str) -> Result<String, HarnessError> {
        self.run_with_cancellation(prompt, CancellationToken::new())
            .await
    }

    /// Run with an external cancellation token. Cancellation propagates to
    /// the in-flight model request and every in-flight tool.
    pub async fn run_with_cancellation(
        &self,
        prompt: &str,
        cancellation: CancellationToken,
    ) -> Result<String, HarnessError> {
        let mut context = Context::new();
        if let Some(system) = &self.system_prompt {
            context.push_system(system.clone());
        }
        context.push_user(prompt);
        self.run_context(&mut context, cancellation).await
    }

    /// Run from a pre-built context. The final context (including the tool
    /// transcript) is left in `context` for the host to inspect or persist.
    pub async fn run_context(
        &self,
        context: &mut Context,
        cancellation: CancellationToken,
    ) -> Result<String, HarnessError> {
        let env = LoopEnv {
            model: &self.model,
            tools: &self.tools,
            extensions: &self.extensions,
            dispatcher: &self.dispatcher,
            limits: &self.limits,
            cancellation,
        };
        agent_loop::run(env, context).await
    }
}
