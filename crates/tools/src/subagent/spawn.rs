//! Shared construction and admission for foreground, detached, and DAG agents.
use super::*;
pub(crate) struct SpawnRequest {
    pub id: u64,
    pub generation: Option<u64>,
    pub expected_model_key: Option<Value>,
    pub depth: Option<u32>,
    pub task: String,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub call_id: String,
    pub run: Option<orca_harness_dag::RunId>,
    pub stage: Option<orca_harness_dag::StageId>,
    pub parent_id: Option<u64>,
    pub notifier: Option<Arc<dyn Fn(SubagentNotification) + Send + Sync>>,
}
pub(crate) struct Prepared<M: Model + Clone + 'static> {
    pub(super) agent: Agent<M>,
    pub(super) spawn: SubagentSpawn,
    pub(super) telemetry: Meter,
    pub(super) started: std::time::Instant,
    pub(super) limits: Limits,
    pub(super) timeout: u32,
    pub(super) background: Option<(BackgroundConfig, background::manager::Admission)>,
    pub(super) in_flight: InFlight,
}
impl<M: Model + Clone + 'static> Prepared<M> {
    pub(crate) fn detach(self) -> BackgroundAcknowledgement {
        detach_subagent(
            self.background.expect("detached admission"),
            self.agent,
            self.spawn,
            self.telemetry,
            self.limits,
            self.timeout,
            self.in_flight,
        )
    }
}
impl<M: Model + Clone + 'static> SubagentTool<M> {
    pub(crate) fn workflow_worker(&self) -> Self {
        let mut worker = self.clone();
        if let Some(config) = &mut worker.background {
            config.manager = config.manager.worker_handle();
        }
        worker
    }
    pub(crate) fn workflow_deadline(&self) -> Option<tokio::time::Instant> {
        execution_deadline(self.limits.deadline, self.max_depth.timeout_secs())
    }
    pub(crate) fn replay_model_key(&self, requested: Option<&str>) -> Value {
        let selected = requested.map(str::to_string).or_else(|| {
            match self.max_depth.effective_model_route() {
                ModelRoute::Fixed(id) => Some(id),
                _ => None,
            }
        });
        self.model_key_for_selection(selected)
    }
    fn model_key_for_selection(&self, selected: Option<String>) -> Value {
        let identity = selected
            .as_ref()
            .and_then(|id| self.models.iter().find(|m| &m.id == id))
            .and_then(|m| m.identity.as_ref())
            .or(self.inherited_identity.as_ref());
        json!({"selected":selected,"identity":identity,"system":self.system_prompt,"modelType":std::any::type_name::<M>()})
    }
    pub(crate) fn next_spawn_id(&self) -> u64 {
        self.spawn_seq.fetch_add(1, Ordering::SeqCst)
    }
    pub(crate) fn background_config(&self) -> Option<&BackgroundConfig> {
        self.background.as_ref()
    }
    pub(crate) fn announce(&self, spawn: &SubagentSpawn) {
        if let Some(factory) = &self.spawn_extensions {
            let _ = factory(spawn);
        }
    }
    pub(crate) fn validate_model(&self, requested: Option<&str>) -> Result<(), ToolError> {
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
        if let ModelRoute::Fixed(id) = &route {
            if !self.models.iter().any(|model| &model.id == id) {
                return Err(ToolError::msg(format!("unknown subagent model `{id}`")));
            }
        }
        Ok(())
    }
    pub(crate) fn prepare_spawn(
        &self,
        req: SpawnRequest,
        detached: bool,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Prepared<M>, ToolError> {
        let requested = req.model.as_deref();
        self.validate_model(requested)?;
        let route = self.max_depth.effective_model_route();
        let selected = requested.map(str::to_string).or(match route {
            ModelRoute::Fixed(preferred) => Some(preferred),
            _ => None,
        });
        if req
            .expected_model_key
            .as_ref()
            .is_some_and(|expected| expected != &self.model_key_for_selection(selected.clone()))
        {
            return Err(ToolError::msg(
                "workflow model route changed; submit a fresh run",
            ));
        }
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
        let system = req.system_prompt.or_else(|| self.system_prompt.clone());

        let spawn_id = req.id;

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
        if detached {
            limits.deadline = limits.deadline.into_iter().chain(deadline).min();
        }
        if !detached {
            limits.deadline =
                execution_deadline(limits.deadline.into_iter().chain(deadline).min(), timeout);
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
            parent_id: req.parent_id.or(self.parent_spawn),
            depth: req.depth.unwrap_or(self.depth),
            call_id: req.call_id,
            task: req.task,
            run: req.run,
            stage: req.stage.clone(),
            identity: identity.clone(),
        };
        // Reserve delivery capacity before host extensions announce this spawn.
        let mut background = self
            .background
            .as_ref()
            .filter(|_| detached)
            .map(|config| {
                match req.generation {
                    Some(generation) => config.manager.inner.admit_stage(&spawn, generation),
                    None => config.admit(&spawn),
                }
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

        if let (Some(notifier), Some((config, _))) = (req.notifier, background.as_mut()) {
            config.notifier = notifier;
        }
        self.stats.inc_agents();
        Ok(Prepared {
            agent,
            spawn,
            telemetry,
            started,
            limits,
            timeout,
            background,
            in_flight: InFlight(self.stats.clone()),
        })
    }
}
