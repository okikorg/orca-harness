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

/// Default governance for workers: enough room for a focused tool task, but
/// deliberately below the orchestrator's budget.
pub const DEFAULT_SUBAGENT_MAX_STEPS: u32 = 12;
pub const DEFAULT_SUBAGENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Bounds on subagent nesting depth. Each extra level multiplies model
/// calls, so the ceiling is deliberately low.
pub const MIN_SUBAGENT_DEPTH: u32 = 1;
pub const MAX_SUBAGENT_DEPTH: u32 = 5;
pub const MIN_SUBAGENT_MAX_STEPS: u32 = 1;
pub const MAX_SUBAGENT_MAX_STEPS: u32 = 48;
pub const MIN_SUBAGENT_TIMEOUT_SECS: u32 = 30;
pub const MAX_SUBAGENT_TIMEOUT_SECS: u32 = 30 * 60;
pub const MIN_SUBAGENT_OUTPUT_CHARS: u32 = 1_000;
pub const MAX_SUBAGENT_OUTPUT_CHARS: u32 = 64_000;
pub const MAX_SUBAGENT_RETRY_ATTEMPTS: u32 = 10;
pub const MAX_SUBAGENT_RETRY_BACKOFF_MS: u32 = 2_000;

#[derive(Debug, Clone, Copy)]
struct LiveRetry {
    attempts: u32,
    backoff_ms: u32,
    configured: bool,
}

#[derive(Debug, Default)]
struct ModelRouting {
    route: Option<String>,
    preferred: std::collections::HashMap<String, String>,
    available: Vec<String>,
}

#[derive(Debug)]
struct SubagentSettingsInner {
    depth: AtomicU32,
    max_steps: AtomicU32,
    timeout_secs: AtomicU32,
    output_chars: AtomicU32,
    retry: StdMutex<LiveRetry>,
    routing: StdMutex<ModelRouting>,
}

/// Shared, session-live governance for spawned agents. The historical name is
/// retained for API compatibility; `/subagents` now adjusts every field.
#[derive(Clone, Debug)]
pub struct SubagentDepth(Arc<SubagentSettingsInner>);

impl Default for SubagentDepth {
    fn default() -> Self {
        Self::new(MIN_SUBAGENT_DEPTH)
    }
}

impl SubagentDepth {
    pub fn new(max_depth: u32) -> Self {
        Self(Arc::new(SubagentSettingsInner {
            depth: AtomicU32::new(clamp_depth(max_depth)),
            max_steps: AtomicU32::new(DEFAULT_SUBAGENT_MAX_STEPS),
            timeout_secs: AtomicU32::new(DEFAULT_SUBAGENT_TIMEOUT.as_secs() as u32),
            output_chars: AtomicU32::new(8_000),
            retry: StdMutex::new(LiveRetry {
                attempts: 1,
                backoff_ms: 250,
                configured: false,
            }),
            routing: StdMutex::new(ModelRouting::default()),
        }))
    }

    pub fn get(&self) -> u32 {
        self.0.depth.load(Ordering::Relaxed)
    }

    pub fn set(&self, max_depth: u32) -> u32 {
        let clamped = clamp_depth(max_depth);
        self.0.depth.store(clamped, Ordering::Relaxed);
        clamped
    }

    pub fn max_steps(&self) -> u32 {
        self.0.max_steps.load(Ordering::Relaxed)
    }

    pub fn set_max_steps(&self, value: u32) -> u32 {
        set_min(&self.0.max_steps, value, MIN_SUBAGENT_MAX_STEPS)
    }

    pub fn timeout_secs(&self) -> u32 {
        self.0.timeout_secs.load(Ordering::Relaxed)
    }

    pub fn set_timeout_secs(&self, value: u32) -> u32 {
        set_min(&self.0.timeout_secs, value, MIN_SUBAGENT_TIMEOUT_SECS)
    }

    pub fn output_chars(&self) -> u32 {
        self.0.output_chars.load(Ordering::Relaxed)
    }

    pub fn set_output_chars(&self, value: u32) -> u32 {
        set_min(&self.0.output_chars, value, MIN_SUBAGENT_OUTPUT_CHARS)
    }

    pub fn tool_attempts(&self) -> u32 {
        self.0.retry.lock().unwrap().attempts
    }

    pub fn set_tool_attempts(&self, value: u32) -> u32 {
        let value = value.max(1);
        let mut retry = self.0.retry.lock().unwrap();
        retry.attempts = value;
        retry.configured = true;
        value
    }

    pub fn retry_backoff_ms(&self) -> u32 {
        self.0.retry.lock().unwrap().backoff_ms
    }

    pub fn set_retry_backoff_ms(&self, value: u32) -> u32 {
        let mut retry = self.0.retry.lock().unwrap();
        retry.backoff_ms = value;
        retry.configured = true;
        value
    }

    /// Install host retry defaults once without overwriting later live choices.
    pub fn ensure_retry_defaults(&self, attempts: u32, backoff_ms: u32) {
        let mut retry = self.0.retry.lock().unwrap();
        if !retry.configured {
            retry.attempts = attempts.max(1);
            retry.backoff_ms = backoff_ms;
            retry.configured = true;
        }
    }

    pub fn model_route(&self) -> Option<String> {
        self.0.routing.lock().unwrap().route.clone()
    }

    pub fn set_model_route(&self, route: Option<String>) -> bool {
        let mut routing = self.0.routing.lock().unwrap();
        if route
            .as_ref()
            .is_some_and(|tier| models_for_tier(&routing.available, tier).is_empty())
        {
            return false;
        }
        routing.route = route;
        true
    }

    pub fn preferred_model(&self, tier: &str) -> Option<String> {
        self.0.routing.lock().unwrap().preferred.get(tier).cloned()
    }

    pub fn set_preferred_model(&self, tier: &str, model: String) -> bool {
        let mut routing = self.0.routing.lock().unwrap();
        if !models_for_tier(&routing.available, tier).contains(&model) {
            return false;
        }
        routing.preferred.insert(tier.to_string(), model);
        true
    }

    pub fn models_for_tier(&self, tier: &str) -> Vec<String> {
        models_for_tier(&self.0.routing.lock().unwrap().available, tier)
    }

    pub fn default_model(&self) -> Option<String> {
        let routing = self.0.routing.lock().unwrap();
        routing
            .route
            .as_ref()
            .and_then(|tier| routing.preferred.get(tier))
            .cloned()
    }

    pub fn set_default_model(&self, model: Option<String>) -> bool {
        let mut routing = self.0.routing.lock().unwrap();
        let Some(model) = model else {
            routing.route = None;
            return true;
        };
        let Some((tier, _)) = model.split_once('/') else {
            return false;
        };
        let tier = tier.to_string();
        if !models_for_tier(&routing.available, &tier).contains(&model) {
            return false;
        }
        routing.preferred.insert(tier.clone(), model);
        routing.route = Some(tier);
        true
    }

    pub fn available_models(&self) -> Vec<String> {
        self.0.routing.lock().unwrap().available.clone()
    }

    fn set_available_models(&self, models: Vec<String>) {
        let mut routing = self.0.routing.lock().unwrap();
        routing.available = models;
        for tier in ["local", "flash", "mid", "frontier"] {
            let choices = models_for_tier(&routing.available, tier);
            match routing.preferred.get(tier) {
                Some(current) if choices.contains(current) => {}
                _ => match choices.first() {
                    Some(first) => {
                        routing.preferred.insert(tier.into(), first.clone());
                    }
                    None => {
                        routing.preferred.remove(tier);
                    }
                },
            }
        }
        if routing
            .route
            .as_ref()
            .is_some_and(|tier| models_for_tier(&routing.available, tier).is_empty())
        {
            routing.route = None;
        }
    }
}

fn models_for_tier(models: &[String], tier: &str) -> Vec<String> {
    let prefix = format!("{tier}/");
    models
        .iter()
        .filter(|id| id.starts_with(&prefix))
        .cloned()
        .collect()
}

fn set_min(target: &AtomicU32, value: u32, min: u32) -> u32 {
    let value = value.max(min);
    target.store(value, Ordering::Relaxed);
    value
}

fn clamp_depth(depth: u32) -> u32 {
    depth.clamp(MIN_SUBAGENT_DEPTH, MAX_SUBAGENT_DEPTH)
}

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
            depth
                .0
                .max_steps
                .store(self.limits.max_steps, Ordering::Relaxed);
        }
        if let Some((attempts, backoff)) = self.retry_policy {
            let mut retry = depth.0.retry.lock().unwrap();
            retry.attempts = attempts;
            retry.backoff_ms = backoff.as_millis().min(u32::MAX as u128) as u32;
            retry.configured = true;
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
        {
            let mut retry = self.max_depth.0.retry.lock().unwrap();
            retry.attempts = max_attempts.max(1);
            retry.backoff_ms = backoff.as_millis().min(u32::MAX as u128) as u32;
            retry.configured = true;
        }
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
        {
            let mut retry = self.max_depth.0.retry.lock().unwrap();
            retry.attempts = max_attempts.max(1);
            retry.backoff_ms = backoff.as_millis().min(u32::MAX as u128) as u32;
            retry.configured = true;
        }
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

/// Minimal usage accumulator for the inner agent. Local to this module:
/// pulling in the extensions crate for one hook would invert the crate
/// layering.
#[derive(Clone, Default)]
struct Meter {
    usage: Arc<StdMutex<Usage>>,
    steps: Arc<AtomicU32>,
    tool_calls: Arc<AtomicU32>,
}

impl Meter {
    fn total(&self) -> Usage {
        *self.usage.lock().unwrap()
    }

    fn steps(&self) -> u32 {
        self.steps.load(Ordering::Relaxed)
    }

    fn tool_calls(&self) -> u32 {
        self.tool_calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Extension for Meter {
    fn name(&self) -> &str {
        "subagent_usage"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_model().after_model()
    }

    async fn before_model(&self, _context: &mut Context) -> Result<(), ExtensionError> {
        self.steps.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn after_model(
        &self,
        _context: &mut Context,
        response: &ModelResponse,
    ) -> Result<(), ExtensionError> {
        if let Some(usage) = response.usage() {
            self.usage.lock().unwrap().add(usage);
        }
        if let ModelResponse::ToolCalls { calls, .. } = response {
            self.tool_calls
                .fetch_add(calls.len() as u32, Ordering::Relaxed);
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
