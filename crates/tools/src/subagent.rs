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

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::{
    Agent, Concurrency, Context, Extension, ExtensionError, Limits, Model, ModelResponse, Next,
    Subscriptions, Tool, ToolCall, ToolContext, ToolError, ToolSchema, Usage,
};

use crate::{core_tools, BackgroundStats, Workspace};

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

/// Predicate marking an `Ok` tool result as a failure for retry purposes.
type OkFailureRule = Arc<dyn Fn(&ToolCall, &Value) -> bool + Send + Sync>;

/// Inner-agent retry policy: total attempts plus backoff.
type RetryPolicy = (u32, std::time::Duration);

/// Identity of one spawned inner agent, handed to the host's
/// spawn-extension factory.
#[derive(Clone, Debug)]
pub struct SubagentSpawn {
    /// Unique across all depths of one tool family (shared counter).
    pub id: u64,
    /// The spawn whose inner agent issued this call; `None` at depth 0.
    pub parent_id: Option<u64>,
    /// 0 = spawned by the top-level agent.
    pub depth: u32,
    /// The spawning agent's tool-call id (anchors UI rendering).
    pub call_id: String,
    pub task: String,
}

/// Builds extensions to attach to each spawned inner agent.
pub type SpawnExtensions = Arc<dyn Fn(&SubagentSpawn) -> Vec<Arc<dyn Extension>> + Send + Sync>;

/// Decrements the in-flight agent count however the call ends.
struct InFlight(BackgroundStats);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.dec_agents();
    }
}

pub struct SubagentTool<M: Model + Clone + 'static> {
    model: M,
    tools: ToolFactory,
    limits: Limits,
    system_prompt: Option<String>,
    /// Distance from the top-level agent; the instance registered there
    /// is depth 0.
    depth: u32,
    max_depth: SubagentDepth,
    spawn_extensions: Option<SpawnExtensions>,
    /// Shared across replicas so ids are unique through the whole tree.
    spawn_seq: Arc<AtomicU64>,
    /// The spawn that created this tool instance (None on the top level).
    parent_spawn: Option<u64>,
    stats: BackgroundStats,
    /// Inner-agent retry: `(max_attempts, backoff)`. Inherited by
    /// replicas. `None` means subagent tool calls are not retried.
    retry_policy: Option<RetryPolicy>,
    /// Data-failure rule for [`Self::retry_policy`].
    ok_failure: Option<OkFailureRule>,
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
            spawn_extensions: None,
            spawn_seq: Arc::new(AtomicU64::new(0)),
            parent_spawn: None,
            stats: BackgroundStats::default(),
            retry_policy: None,
            ok_failure: None,
        }
    }

    /// Attach host extensions (event streams, policy, ...) to every
    /// spawned inner agent, including nested ones.
    pub fn spawn_extensions(mut self, factory: SpawnExtensions) -> Self {
        self.spawn_extensions = Some(factory);
        self
    }

    /// Adopt shared live counters (in-flight agent count, all depths).
    pub fn stats(mut self, stats: BackgroundStats) -> Self {
        self.stats = stats;
        self
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

    /// Configure the inner-agent retry policy: `max_attempts` total tries,
    /// then a `backoff` between attempts. Any `Ok` result matching
    /// `ok_failure` is retried too (data failures like a nonzero shell
    /// exit). Pass `None` to keep retry-on-`Err`-only. Hosts that want
    /// subagents to share the top-level retry toggle call this from their
    /// agent build (the CLI does).
    pub fn retry(mut self, max_attempts: u32, backoff: std::time::Duration) -> Self {
        self.retry_policy = Some((max_attempts.max(1), backoff));
        self
    }

    /// Like [`Self::retry`], with a data-failure rule.
    pub fn retry_with_rule(
        mut self,
        max_attempts: u32,
        backoff: std::time::Duration,
        ok_failure: impl Fn(&ToolCall, &Value) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.retry_policy = Some((max_attempts.max(1), backoff));
        self.ok_failure = Some(std::sync::Arc::new(ok_failure));
        self
    }

    fn child_replica(&self, spawn_id: u64) -> Self {
        Self {
            model: self.model.clone(),
            tools: self.tools.clone(),
            limits: self.limits.clone(),
            system_prompt: self.system_prompt.clone(),
            depth: self.depth + 1,
            max_depth: self.max_depth.clone(),
            spawn_extensions: self.spawn_extensions.clone(),
            spawn_seq: self.spawn_seq.clone(),
            parent_spawn: Some(spawn_id),
            stats: self.stats.clone(),
            retry_policy: self.retry_policy,
            ok_failure: self.ok_failure.clone(),
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

/// Tool-call retry for *inner* agents. Deliberately a local copy of the
/// extensions crate's `ToolRetry` semantics (crate-layering:
/// `orca-harness-tools` must not depend on `orca-harness-extensions`):
/// re-invoke a failing call up to `max_attempts` times, and treat an
/// `Ok` result the [`OkFailureRule`] rejects as a failed attempt too.
#[derive(Clone)]
struct SubagentRetry {
    max_attempts: u32,
    backoff: std::time::Duration,
    /// Data-failure rule for the inherited [`RetryPolicy`].
    ok_failure: Option<OkFailureRule>,
}

impl SubagentRetry {
    fn new(policy: RetryPolicy, ok_failure: Option<OkFailureRule>) -> Self {
        Self {
            max_attempts: policy.0,
            backoff: policy.1,
            ok_failure,
        }
    }
}

#[async_trait]
impl Extension for SubagentRetry {
    fn name(&self) -> &str {
        "subagent-tool-retry"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().around_tool()
    }

    async fn around_tool<'a>(
        &self,
        call: &ToolCall,
        input: Value,
        _ctx: &ToolContext,
        next: Next<'a>,
    ) -> Result<Value, ToolError> {
        let mut last_err = None;
        let mut last_value = None;
        for attempt in 1..=self.max_attempts {
            match next.run(input.clone()).await {
                Ok(value) => {
                    if self
                        .ok_failure
                        .as_ref()
                        .is_some_and(|rule| rule(call, &value))
                    {
                        last_value = Some(value);
                        if attempt == self.max_attempts {
                            break;
                        }
                        tokio::time::sleep(self.backoff).await;
                        continue;
                    }
                    return Ok(value);
                }
                Err(err) => {
                    last_err = Some(err);
                    if attempt < self.max_attempts {
                        tokio::time::sleep(self.backoff).await;
                    }
                }
            }
        }
        match last_value {
            Some(value) => Ok(value),
            None => {
                Err(last_err.unwrap_or_else(|| ToolError::msg("subagent retry: no attempts made")))
            }
        }
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

        let spawn_id = self.spawn_seq.fetch_add(1, Ordering::SeqCst);
        self.stats.inc_agents();
        let _in_flight = InFlight(self.stats.clone());

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
            agent = agent.tool_arc(Arc::new(self.child_replica(spawn_id)));
        }
        if let Some(factory) = &self.spawn_extensions {
            let spawn = SubagentSpawn {
                id: spawn_id,
                parent_id: self.parent_spawn,
                depth: self.depth,
                call_id: ctx.call_id.clone(),
                task: task.to_string(),
            };
            for extension in factory(&spawn) {
                agent = agent.extension_arc(extension);
            }
        }
        // Retry *inside* the inner loop. The top-level agent's `ToolRetry`
        // only wraps that agent's own tool calls — inner agents build a
        // fresh `Agent` here, so without this they get no retry at all.
        // Register after the host's spawn extensions: like the top-level
        // build, retry wraps their `around_tool`, and denials from
        // `before_tool` never reach the around chain, so a `Deny` verdict
        // is not retried.
        if let Some(policy) = self.retry_policy {
            agent = agent.extension_arc(std::sync::Arc::new(SubagentRetry::new(
                policy,
                self.ok_failure.clone(),
            )));
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
