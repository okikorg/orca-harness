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

mod settings;
pub use settings::*;

type ToolFactory = Arc<dyn Fn() -> Vec<Arc<dyn Tool>> + Send + Sync>;

/// Predicate marking an `Ok` tool result as a failure for retry purposes.
type OkFailureRule = Arc<dyn Fn(&ToolCall, &Value) -> bool + Send + Sync>;

/// Inner-agent retry policy: total attempts plus backoff.
type RetryPolicy = (u32, std::time::Duration);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentIdentity {
    pub provider: String,
    pub model: String,
    /// Stable tool-facing route such as `frontier/claude-sonnet-5`.
    /// `None` means the worker inherited the orchestrator model.
    pub route: Option<String>,
}

impl SubagentIdentity {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            route: None,
        }
    }

    fn with_route(mut self, route: String) -> Self {
        self.route = Some(route);
        self
    }
}

/// One host-approved model the orchestrator may select for a spawned agent.
/// The host owns provider construction and credentials; this tool only exposes
/// the stable `id` and `description` to the model.
#[derive(Clone)]
pub struct SubagentModel<M> {
    pub id: String,
    pub description: String,
    pub model: M,
    pub identity: Option<SubagentIdentity>,
}

impl<M> SubagentModel<M> {
    pub fn new(id: impl Into<String>, description: impl Into<String>, model: M) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
            model,
            identity: None,
        }
    }

    pub fn identity(mut self, provider: impl Into<String>, model: impl Into<String>) -> Self {
        self.identity = Some(SubagentIdentity::new(provider, model));
        self
    }
}

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
    pub identity: Option<SubagentIdentity>,
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
    inherited_identity: Option<SubagentIdentity>,
    models: Vec<SubagentModel<M>>,
    tools: ToolFactory,
    limits: Limits,
    limits_configured: bool,
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
            inherited_identity: None,
            models: Vec::new(),
            tools,
            // Focused workers must synthesize rather than explore until the
            // orchestrator's larger budget is exhausted.
            limits: Limits {
                max_steps: DEFAULT_SUBAGENT_MAX_STEPS,
                ..Limits::default()
            },
            limits_configured: false,
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
        self.limits_configured = true;
        self
    }

    /// Name the model inherited when a call has no explicit or configured
    /// route. Hosts that omit this still run normally, but cannot surface a
    /// resolved provider/model for inherited workers.
    pub fn inherited_identity(
        mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.inherited_identity = Some(SubagentIdentity::new(provider, model));
        self
    }

    /// Offer a host-approved model shortlist to the orchestrator. Omitting
    /// `model` in a call continues to use the model passed to [`Self::new`].
    pub fn models(mut self, models: impl IntoIterator<Item = SubagentModel<M>>) -> Self {
        self.models = models.into_iter().collect();
        self.max_depth
            .set_available_models(self.models.iter().map(|model| model.id.clone()).collect());
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
        depth.set_available_models(self.models.iter().map(|model| model.id.clone()).collect());
        if self.limits_configured {
            depth.set_max_steps_from_limits(self.limits.max_steps);
        }
        if let Some((attempts, backoff)) = self.retry_policy {
            depth.configure_retry(attempts, backoff.as_millis().min(u32::MAX as u128) as u32);
        }
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
        self.max_depth.configure_retry(
            max_attempts.max(1),
            backoff.as_millis().min(u32::MAX as u128) as u32,
        );
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
        self.max_depth.configure_retry(
            max_attempts.max(1),
            backoff.as_millis().min(u32::MAX as u128) as u32,
        );
        self.ok_failure = Some(std::sync::Arc::new(ok_failure));
        self
    }

    /// Set only the data-failure classifier. Retry attempts and backoff remain
    /// controlled by the shared live settings handle.
    pub fn retry_ok_when(
        mut self,
        ok_failure: impl Fn(&ToolCall, &Value) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.ok_failure = Some(std::sync::Arc::new(ok_failure));
        self
    }

    fn child_replica(&self, spawn_id: u64, model: M, identity: Option<SubagentIdentity>) -> Self {
        Self {
            model,
            inherited_identity: identity,
            models: self.models.clone(),
            tools: self.tools.clone(),
            limits: self.limits.clone(),
            limits_configured: self.limits_configured,
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

include!("subagent/telemetry_retry.rs");

#[async_trait]
impl<M: Model + Clone + 'static> Tool for SubagentTool<M> {
    fn schema(&self) -> ToolSchema {
        let mut properties = serde_json::Map::from_iter([
            (
                "task".into(),
                json!({"type": "string", "description": "Complete, self-contained task for the agent."}),
            ),
            (
                "systemPrompt".into(),
                json!({"type": "string", "description": "Optional system prompt override for this agent."}),
            ),
        ]);
        if !self.models.is_empty() {
            let ids: Vec<&str> = self
                .models
                .iter()
                .map(|choice| choice.id.as_str())
                .collect();
            let choices = self
                .models
                .iter()
                .map(|choice| format!("{} — {}", choice.id, choice.description))
                .collect::<Vec<_>>()
                .join("; ");
            properties.insert(
                "model".into(),
                json!({
                    "type": "string",
                    "enum": ids,
                    "description": format!(
                        "Optional worker model. Omit to use the orchestrator's current model. Choices: {choices}"
                    )
                }),
            );
        }

        ToolSchema {
            name: "subagent".into(),
            description: "Spawn an independent agent with its own context and full file/shell \
                tool access to work on one bounded task. Subagents can run for many model steps, \
                so delegate deliberately: give a complete, self-contained task with the exact \
                result expected and an explicit stopping condition. Avoid open-ended goals or \
                investigation without a defined deliverable. It sees nothing of this conversation \
                and returns only its final answer. Several subagent calls issued in the same \
                response run in parallel."
                .into(),
            parameters: Value::Object(serde_json::Map::from_iter([
                ("type".into(), Value::String("object".into())),
                ("properties".into(), Value::Object(properties)),
                ("required".into(), json!(["task"])),
            ])),
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
        let requested = input.get("model").and_then(Value::as_str);
        let selected = requested
            .map(str::to_string)
            .or_else(|| self.max_depth.default_model());
        let (model, identity) = match selected.as_deref() {
            None => (self.model.clone(), self.inherited_identity.clone()),
            Some(id) => {
                let choice = self
                    .models
                    .iter()
                    .find(|choice| choice.id == id)
                    .ok_or_else(|| ToolError::msg(format!("unknown subagent model `{id}`")))?;
                (
                    choice.model.clone(),
                    choice
                        .identity
                        .clone()
                        .map(|identity| identity.with_route(id.to_string())),
                )
            }
        };
        let system = input
            .get("systemPrompt")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.system_prompt.clone());

        let spawn_id = self.spawn_seq.fetch_add(1, Ordering::SeqCst);
        self.stats.inc_agents();
        let _in_flight = InFlight(self.stats.clone());

        let meter = Meter::default();
        let telemetry = meter.clone();
        let started = std::time::Instant::now();
        let mut limits = self.limits.clone();
        if !self.limits_configured {
            limits.max_steps = self.max_depth.max_steps();
        }
        let worker_deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(self.max_depth.timeout_secs() as u64);
        limits.deadline = Some(match (limits.deadline, ctx.deadline) {
            (Some(configured), Some(parent)) => configured.min(parent).min(worker_deadline),
            (Some(configured), None) => configured.min(worker_deadline),
            (None, Some(parent)) => parent.min(worker_deadline),
            (None, None) => worker_deadline,
        });
        let mut agent = Agent::new(model.clone()).limits(limits).extension(meter);
        if let Some(system) = system {
            agent = agent.system_prompt(system);
        }
        for tool in (self.tools)() {
            agent = agent.tool_arc(tool);
        }
        if self.depth + 1 < self.max_depth.get() {
            agent = agent.tool_arc(Arc::new(self.child_replica(
                spawn_id,
                model,
                identity.clone(),
            )));
        }
        if let Some(factory) = &self.spawn_extensions {
            let spawn = SubagentSpawn {
                id: spawn_id,
                parent_id: self.parent_spawn,
                depth: self.depth,
                call_id: ctx.call_id.clone(),
                task: task.to_string(),
                identity: identity.clone(),
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
        let tool_attempts = self.max_depth.tool_attempts();
        if tool_attempts > 1 {
            agent = agent.extension_arc(std::sync::Arc::new(SubagentRetry::new(
                (
                    tool_attempts,
                    std::time::Duration::from_millis(self.max_depth.retry_backoff_ms() as u64),
                ),
                self.ok_failure.clone(),
            )));
        }

        let result = agent
            .run_with_cancellation(task, ctx.cancellation.child_token())
            .await;
        let elapsed_ms = started.elapsed().as_millis();
        let total = telemetry.total();
        let steps = telemetry.steps();
        let tool_calls = telemetry.tool_calls();
        let answer = result.map_err(|err| {
            ToolError::msg(format!(
                "subagent failed: {err} (runtimeMs={elapsed_ms}, steps={steps}, toolCalls={tool_calls}, inputTokens={}, outputTokens={})",
                total.input_tokens, total.output_tokens
            ))
        })?;
        Ok(json!({
            "answer": answer,
            "usage": {
                "inputTokens": total.input_tokens,
                "outputTokens": total.output_tokens,
            },
            "runtimeMs": elapsed_ms,
            "steps": steps,
            "toolCalls": tool_calls,
            "termination": "completed",
            "identity": identity.as_ref().map(|identity| json!({
                "provider": identity.provider,
                "model": identity.model,
                "route": identity.route,
            })),
        }))
    }
}
