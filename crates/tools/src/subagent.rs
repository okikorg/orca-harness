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
//! dies with it. Foreground workers follow parent cancellation; detached
//! workers follow their session manager lifetime.

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

mod background;
use background::{
    detach_subagent, execution_deadline, subagent_control, subagent_parameters, subagent_result,
    BackgroundConfig, InFlight,
};
pub use background::{
    BackgroundJob, BackgroundStatus, SubagentManager, SubagentNotification,
    DEFAULT_BACKGROUND_SUBAGENT_LIMIT,
};

enum ModelRoute {
    Inherit,
    Auto,
    Preference(Vec<String>),
    Fixed(String),
}

mod identity;
pub use identity::SubagentIdentity;

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
    background: Option<BackgroundConfig>,
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
            background: None,
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

    /// Enable explicit `background: true` calls for an interactive host.
    pub fn background(
        mut self,
        manager: SubagentManager,
        notify: impl Fn(SubagentNotification) + Send + Sync + 'static,
    ) -> Self {
        self.spawn_seq = manager.spawn_sequence();
        self.background = Some(BackgroundConfig {
            manager,
            notifier: Arc::new(notify),
            last_list: Default::default(),
        });
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

    /// Offer host-approved models for `/subagents` routing. `inherit` hides
    /// the shortlist, `auto` exposes it, `preference` exposes only saved
    /// preferred models, and a fixed route exposes its preferred model.
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
            // Detached nesting needs durable child-context ownership. Keep the
            // first release depth-zero while preserving foreground nesting.
            background: None,
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
        let route = self.max_depth.effective_model_route();
        let requires_model = matches!(&route, ModelRoute::Preference(_));
        let visible_models: Vec<&SubagentModel<M>> = match &route {
            ModelRoute::Inherit => Vec::new(),
            ModelRoute::Auto => self.models.iter().collect(),
            ModelRoute::Preference(preferred) => self
                .models
                .iter()
                .filter(|choice| preferred.contains(&choice.id))
                .collect(),
            ModelRoute::Fixed(preferred) => self
                .models
                .iter()
                .filter(|choice| choice.id == *preferred)
                .collect(),
        };
        if !visible_models.is_empty() || requires_model {
            let ids = visible_models
                .iter()
                .map(|choice| choice.id.as_str())
                .collect::<Vec<_>>();
            let choices = visible_models
                .iter()
                .map(|choice| format!("{} — {}", choice.id, choice.description))
                .collect::<Vec<_>>()
                .join("; ");
            let description = if visible_models.is_empty() {
                "Required worker model, but no saved preferred model is currently available."
                    .to_string()
            } else if requires_model {
                format!("Required user-preferred worker model. Choices: {choices}")
            } else {
                format!("Optional worker model. Choices: {choices}")
            };
            properties.insert(
                "model".into(),
                json!({
                    "type": "string",
                    "enum": ids,
                    "description": description
                }),
            );
        }
        let routing = match &route {
            ModelRoute::Inherit => " The user's `/subagents` route is `inherit`; omit `model`. \
                Every worker uses the orchestrator's current model, and explicit model requests \
                are rejected."
                .to_string(),
            ModelRoute::Auto => " The user's `/subagents` route is `auto`; choose any approved \
                worker model per task, or omit `model` to inherit the orchestrator's current \
                model."
                .to_string(),
            ModelRoute::Preference(_) => " The user's `/subagents` route is `preference`; choose \
                exactly one of the user's saved preferred models exposed in `model`. Omitting \
                `model` or requesting any other model is rejected."
                .to_string(),
            ModelRoute::Fixed(preferred) => format!(
                " The user's `/subagents` route is locked to `{preferred}`; omit `model` or pass \
                 exactly `{preferred}`. Conflicting model requests are rejected."
            ),
        };
        let controls = if self.background.is_some() {
            format!(" Background execution is the default; set background=false explicitly to wait in the foreground. A background call returns a spawnId immediately; the answer arrives later as a `background_subagent_completions` message batched with any other results ready at that moment. {BACKGROUND_DELIVERY} The acknowledgement's status is `running`, or `queued` when the user's concurrency limit is reached and the agent starts once a running one finishes. To inspect or stop spawned agents, call this subagent tool, not process: action=list answers what is running right now, action=cancel with spawnId or action=cancel_all stops agents.", BACKGROUND_DELIVERY = background::BACKGROUND_DELIVERY)
        } else {
            String::new()
        };
        ToolSchema {
            name: "subagent".into(),
            description: format!(
                "Spawn an independent agent with its own context and full file/shell \
                tool access to work on one bounded task. Subagents can run for many model steps, \
                so delegate deliberately: give a complete, self-contained task with the exact \
                result expected and an explicit stopping condition. Avoid open-ended goals or \
                investigation without a defined deliverable. It sees nothing of this conversation. \
                A foreground run returns only its final answer. Several subagent calls issued in the same \
                response run in parallel.{routing}{controls}"
            ),
            parameters: subagent_parameters(self.background.as_ref().map(|config| &config.manager), properties, requires_model),
        }
    }

    fn concurrency(&self, _input: &Value) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        if let Some(result) = subagent_control(self.background.as_ref(), &input) {
            return result;
        }
        let task = input
            .get("task")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`task` (string) is required"))?;
        let detached = input
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(self.background.is_some());
        if detached && self.background.is_none() {
            return Err(ToolError::msg(
                "background subagents are unavailable in this host or nesting depth",
            ));
        }
        let requested = input.get("model").and_then(Value::as_str);
        let route = self.max_depth.effective_model_route();
        if let Some(requested) = requested {
            if !self.models.iter().any(|choice| choice.id == requested) {
                return Err(ToolError::msg(format!(
                    "unknown subagent model `{requested}`"
                )));
            }
        }
        match (&route, requested) {
            (ModelRoute::Inherit, Some(requested)) => {
                return Err(ToolError::msg(format!(
                    "subagent model `{requested}` conflicts with the user's `inherit` \
                     preference selected via `/subagents`; omit `model` to use the \
                     orchestrator's current model"
                )));
            }
            (ModelRoute::Fixed(preferred), Some(requested)) if requested != preferred => {
                return Err(ToolError::msg(format!(
                    "subagent model `{requested}` conflicts with the user's preferred model \
                     `{preferred}` selected via `/subagents`; omit `model` or request \
                     `{preferred}`"
                )));
            }
            (ModelRoute::Preference(_), None) => {
                return Err(ToolError::msg(
                    "the user's `preference` route requires one saved preferred `model`",
                ));
            }
            (ModelRoute::Preference(preferred), Some(requested))
                if !preferred.iter().any(|model| model == requested) =>
            {
                return Err(ToolError::msg(format!(
                    "subagent model `{requested}` is not one of the user's saved preferred models"
                )));
            }
            _ => {}
        }
        let selected = requested.map(str::to_string).or(match route {
            ModelRoute::Fixed(preferred) => Some(preferred),
            _ => None,
        });
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

        let meter = Meter::default();
        let telemetry = meter.clone();
        let started = std::time::Instant::now();
        let mut limits = self.limits.clone();
        if !self.limits_configured {
            limits.max_steps = self.max_depth.max_steps();
        }
        if let Some(parallel) = self.max_depth.parallel_tools() {
            limits.max_parallel_tools = if parallel == 0 {
                usize::MAX
            } else {
                parallel as usize
            };
        }
        let timeout = self.max_depth.timeout_secs();
        if !detached {
            limits.deadline = execution_deadline(
                limits.deadline.into_iter().chain(ctx.deadline).min(),
                timeout,
            );
        }
        let mut agent = Agent::new(model.clone())
            .limits(limits.clone())
            .extension(meter);
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
        let spawn = SubagentSpawn {
            id: spawn_id,
            parent_id: self.parent_spawn,
            depth: self.depth,
            call_id: ctx.call_id.clone(),
            task: task.to_string(),
            identity: identity.clone(),
        };
        // Reserve delivery capacity before host extensions announce this spawn.
        let background = self
            .background
            .as_ref()
            .filter(|_| detached)
            .map(|config| {
                config
                    .admit(&spawn)
                    .map(|admission| (config.clone(), admission))
            })
            .transpose()
            .map_err(ToolError::msg)?;
        if let Some(factory) = &self.spawn_extensions {
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

        self.stats.inc_agents();
        let in_flight = InFlight(self.stats.clone());
        if let Some(background) = background {
            return Ok(detach_subagent(
                background, agent, spawn, telemetry, limits, timeout, in_flight,
            ));
        }

        let _in_flight = in_flight;
        let result = agent
            .run_with_cancellation(task, ctx.cancellation.child_token())
            .await;
        subagent_result(result, &telemetry, started, identity.as_ref()).map_err(ToolError::msg)
    }
}
