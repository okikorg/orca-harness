//! `subagent` — spawn an independent in-process agent.
//!
//! Each call builds a fresh [`Agent`](orca_harness_core::Agent) — own
//! context, own loop, own tool set — runs one complete task, and returns
//! only the final answer plus token usage. Calls issued in the same batch
//! fan out in parallel, so one orchestrating model can farm work to N
//! workers at once.
//!
//! Nesting is the tool's own affair: a `SubagentTool` at depth `d` hands
//! its children a depth `d + 1` replica of itself only while
//! `d + 1 < max_depth`, where `max_depth` lives behind a shared
//! [`SubagentDepth`] handle a host can adjust mid-session (the CLI's
//! `/subagents` command). At the limit the child simply has no
//! `subagent` tool — no denials, no recursion.
//!
//! Lifetime: the per-call tool instances (including a fresh `process`
//! manager) drop when the call returns, so anything a subagent spawned
//! dies with it. Cancelling the parent run cancels every level below.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::{
    Agent, Concurrency, Context, Extension, ExtensionError, Limits, Model, ModelResponse,
    Subscriptions, Tool, ToolContext, ToolError, ToolSchema, Usage,
};

use crate::{core_tools, Workspace};

/// Bounds on subagent nesting depth. Each extra level multiplies model
/// calls, so the ceiling is deliberately low.
pub const MIN_SUBAGENT_DEPTH: u32 = 1;
pub const MAX_SUBAGENT_DEPTH: u32 = 5;

/// Shared, host-adjustable cap on subagent nesting. `1` (the default)
/// lets a top-level agent spawn workers that cannot nest further; read
/// at spawn time, so changes apply to the next spawn.
#[derive(Clone, Debug)]
pub struct SubagentDepth(Arc<AtomicU32>);

impl Default for SubagentDepth {
    fn default() -> Self {
        Self::new(MIN_SUBAGENT_DEPTH)
    }
}

impl SubagentDepth {
    pub fn new(max_depth: u32) -> Self {
        Self(Arc::new(AtomicU32::new(clamp_depth(max_depth))))
    }

    pub fn get(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }

    /// Set the cap, clamped to the permitted range; returns the value
    /// actually stored.
    pub fn set(&self, max_depth: u32) -> u32 {
        let clamped = clamp_depth(max_depth);
        self.0.store(clamped, Ordering::Relaxed);
        clamped
    }
}

fn clamp_depth(depth: u32) -> u32 {
    depth.clamp(MIN_SUBAGENT_DEPTH, MAX_SUBAGENT_DEPTH)
}

type ToolFactory = Arc<dyn Fn() -> Vec<Arc<dyn Tool>> + Send + Sync>;

pub struct SubagentTool<M: Model + Clone + 'static> {
    model: M,
    tools: ToolFactory,
    limits: Limits,
    system_prompt: Option<String>,
    /// Distance from the top-level agent; the instance registered there
    /// is depth 0.
    depth: u32,
    max_depth: SubagentDepth,
}

impl<M: Model + Clone + 'static> SubagentTool<M> {
    /// Subagents equipped with [`core_tools`] rooted at `ws`.
    pub fn new(model: M, ws: &Workspace) -> Self {
        let ws = ws.clone();
        Self::with_tools(model, Arc::new(move || core_tools(&ws)))
    }

    /// Subagents equipped with an arbitrary tool set. The factory should
    /// NOT include a `subagent` tool — nesting is added by this tool
    /// itself, governed by [`SubagentDepth`].
    pub fn with_tools(model: M, tools: ToolFactory) -> Self {
        Self {
            model,
            tools,
            // Deliberately below Limits::default() (32): workers get a
            // tighter leash than the orchestrator.
            limits: Limits {
                max_steps: 24,
                ..Limits::default()
            },
            system_prompt: None,
            depth: 0,
            max_depth: SubagentDepth::default(),
        }
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Default system prompt for spawned agents (a call's `systemPrompt`
    /// overrides it).
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Share a depth handle (and thus a runtime-adjustable nesting cap).
    pub fn max_depth(mut self, depth: SubagentDepth) -> Self {
        self.max_depth = depth;
        self
    }

    fn child_replica(&self) -> Self {
        Self {
            model: self.model.clone(),
            tools: self.tools.clone(),
            limits: self.limits.clone(),
            system_prompt: self.system_prompt.clone(),
            depth: self.depth + 1,
            max_depth: self.max_depth.clone(),
        }
    }
}

/// Minimal usage accumulator for the inner agent. Local to this module:
/// pulling in the extensions crate for one hook would invert the crate
/// layering.
#[derive(Clone, Default)]
struct Meter(Arc<StdMutex<Usage>>);

impl Meter {
    fn total(&self) -> Usage {
        *self.0.lock().unwrap()
    }
}

#[async_trait]
impl Extension for Meter {
    fn name(&self) -> &str {
        "subagent_usage"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().after_model()
    }

    async fn after_model(
        &self,
        _context: &mut Context,
        response: &ModelResponse,
    ) -> Result<(), ExtensionError> {
        if let Some(usage) = response.usage() {
            self.0.lock().unwrap().add(usage);
        }
        Ok(())
    }
}

#[async_trait]
impl<M: Model + Clone + 'static> Tool for SubagentTool<M> {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "subagent".into(),
            description: "Spawn an independent agent with its own context and full file/shell \
                tool access to work on one task. Give it a complete, self-contained task \
                description — it sees nothing of this conversation and returns only its final \
                answer. Several subagent calls issued in the same response run in parallel."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "Complete, self-contained task for the agent."},
                    "systemPrompt": {"type": "string", "description": "Optional system prompt override for this agent."}
                },
                "required": ["task"]
            }),
        }
    }

    fn concurrency(&self, _input: &Value) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let task = input
            .get("task")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`task` (string) is required"))?;
        let system = input
            .get("systemPrompt")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.system_prompt.clone());

        let meter = Meter::default();
        let usage = meter.clone();
        let mut agent = Agent::new(self.model.clone())
            .limits(self.limits.clone())
            .extension(meter);
        if let Some(system) = system {
            agent = agent.system_prompt(system);
        }
        for tool in (self.tools)() {
            agent = agent.tool_arc(tool);
        }
        if self.depth + 1 < self.max_depth.get() {
            agent = agent.tool_arc(Arc::new(self.child_replica()));
        }

        let answer = agent
            .run_with_cancellation(task, ctx.cancellation.child_token())
            .await
            .map_err(|e| ToolError::msg(format!("subagent failed: {e}")))?;
        let total = usage.total();
        Ok(json!({
            "answer": answer,
            "usage": {
                "inputTokens": total.input_tokens,
                "outputTokens": total.output_tokens,
            }
        }))
    }
}
