//! Tools: the only way an agent acts on the world.
//!
//! A Tool does not own permissions, telemetry, retries, approvals, memory,
//! MCP lifecycle, or sandbox management — those are Extensions.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;

pub type ToolName = String;

/// How a call may be scheduled relative to other calls in the same batch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Concurrency {
    /// Safe to run alongside anything.
    Parallel,
    /// Must run exclusively: no other call may execute while this one does.
    Serial,
    /// Calls sharing a key are serialized in call order; unrelated calls
    /// continue concurrently.
    Keyed(String),
}

/// Model-facing description of a tool. `parameters` is a JSON Schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Per-call execution context handed to a tool. Tools should observe
/// `cancellation` cooperatively during long operations.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub call_id: String,
    pub tool_name: String,
    pub cancellation: CancellationToken,
    pub deadline: Option<Instant>,
}

/// A single tool invocation requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// The model-visible outcome of one tool call. Pairing (`call_id`) and
/// ordering are kernel guarantees.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub tool_name: String,
    pub output: Value,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(call: &ToolCall, output: Value) -> Self {
        Self {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            output,
            is_error: false,
        }
    }

    pub fn error(call: &ToolCall, message: impl Into<String>) -> Self {
        Self {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            output: serde_json::json!({ "error": message.into() }),
            is_error: true,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn schema(&self) -> ToolSchema;

    /// Classify a specific call. Defaults to [`Concurrency::Parallel`];
    /// override to serialize conflicting operations (e.g. key writes by
    /// target path).
    fn concurrency(&self, _input: &Value) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError>;
}

/// Registry of tools available to one agent. Schemas are exposed in
/// registration order so the model-visible tool list is deterministic.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<ToolName, Arc<dyn Tool>>,
    order: Vec<ToolName>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. Re-registering a name replaces the previous tool
    /// but keeps its original position.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.schema().name;
        if self.tools.insert(name.clone(), tool).is_none() {
            self.order.push(name);
        }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.order
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|tool| tool.schema())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

type BoxToolFuture = Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send>>;
type ToolFn = Box<dyn Fn(Value, ToolContext) -> BoxToolFuture + Send + Sync>;
type ClassifyFn = Box<dyn Fn(&Value) -> Concurrency + Send + Sync>;

/// A tool built from a closure. Convenient for hosts, tests, and examples;
/// production tools usually implement [`Tool`] directly.
pub struct FnTool {
    schema: ToolSchema,
    f: ToolFn,
    classify: Option<ClassifyFn>,
}

impl FnTool {
    pub fn new<F, Fut>(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        f: F,
    ) -> Self
    where
        F: Fn(Value, ToolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, ToolError>> + Send + 'static,
    {
        Self {
            schema: ToolSchema {
                name: name.into(),
                description: description.into(),
                parameters,
            },
            f: Box::new(move |input, ctx| Box::pin(f(input, ctx))),
            classify: None,
        }
    }

    /// Set a per-call concurrency classifier.
    pub fn concurrency<C>(mut self, classify: C) -> Self
    where
        C: Fn(&Value) -> Concurrency + Send + Sync + 'static,
    {
        self.classify = Some(Box::new(classify));
        self
    }
}

#[async_trait]
impl Tool for FnTool {
    fn schema(&self) -> ToolSchema {
        self.schema.clone()
    }

    fn concurrency(&self, input: &Value) -> Concurrency {
        match &self.classify {
            Some(classify) => classify(input),
            None => Concurrency::Parallel,
        }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        (self.f)(input, ctx.clone()).await
    }
}
