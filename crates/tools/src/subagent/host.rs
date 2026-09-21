//! Typed host operations over the same implementation the `subagent`
//! model tool runs.
//!
//! A host (the SDK, an embedding application) spawns and controls
//! workers without composing tool-call JSON or a `ToolContext`. Every
//! entry point here and [`Tool::call`] funnel through one private path:
//! [`SubagentRequest`] -> `SpawnRequest` -> `prepare_spawn` -> detach or
//! run. Host-originated spawns are therefore subject to the same routing
//! validation, admission limits, and spawn extensions as model-originated
//! ones; the only observable difference is the synthetic `host:<spawn id>`
//! call id in [`SubagentSpawn::call_id`].

use super::spawn::{Prepared, SpawnRequest};
use super::*;
use std::collections::{HashMap, HashSet, VecDeque};
use tokio::sync::Mutex;

const SIDEKICK_GUIDANCE: &str = "You are a session-scoped sidekick. The parent owns user intent, ambiguity, planning, decisions, and final review. Perform only the bounded mechanical inspection, edit, or test work delegated to you. Return a short evidence-backed report with relevant paths or results; do not dump raw tool logs. Escalate unresolved judgment or ambiguity to the parent.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidekickStatus {
    Idle,
    Busy,
    Stopped,
}

impl SidekickStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidekickReport {
    pub spawn_id: u64,
    pub status: SidekickStatus,
    pub answer: String,
    pub identity: Option<SubagentIdentity>,
}

/// Immediate acknowledgement for a persistent sidekick task. The stable
/// `spawn_id` names the retained conversation for every later turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidekickAcknowledgement {
    pub spawn_id: u64,
    pub status: BackgroundStatus,
    pub identity: Option<SubagentIdentity>,
}

impl SidekickAcknowledgement {
    pub fn into_value(self) -> Value {
        json!({"spawnId": self.spawn_id, "status": self.status.as_str(),
            "identity": self.identity, "persistent": true, "termination": "detached"})
    }
}

impl SidekickReport {
    pub fn into_value(self) -> Value {
        json!({"spawnId": self.spawn_id, "status": self.status.as_str(), "answer": self.answer,
            "identity": self.identity, "termination": "retained"})
    }
}

pub(crate) struct SidekickRegistry<M: Model + Clone + 'static> {
    inner: Arc<SidekickRegistryInner<M>>,
}

struct SidekickRegistryInner<M: Model + Clone + 'static> {
    live: StdMutex<HashMap<u64, Arc<Sidekick<M>>>>,
    stopped: StdMutex<HashSet<u64>>,
    stopped_order: StdMutex<VecDeque<u64>>,
}

impl<M: Model + Clone + 'static> Clone for SidekickRegistry<M> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<M: Model + Clone + 'static> Default for SidekickRegistry<M> {
    fn default() -> Self {
        Self {
            inner: Arc::new(SidekickRegistryInner {
                live: StdMutex::default(),
                stopped: StdMutex::default(),
                stopped_order: StdMutex::default(),
            }),
        }
    }
}

impl<M: Model + Clone + 'static> SidekickRegistryInner<M> {
    fn remember_stopped(&self, id: u64) {
        let mut stopped = self.stopped.lock().unwrap();
        if stopped.insert(id) {
            let mut order = self.stopped_order.lock().unwrap();
            order.push_back(id);
            if order.len() > 256 {
                if let Some(expired) = order.pop_front() {
                    stopped.remove(&expired);
                }
            }
        }
    }

    fn stop_all(&self) -> usize {
        let sidekicks = std::mem::take(&mut *self.live.lock().unwrap());
        let count = sidekicks.len();
        for (id, sidekick) in sidekicks {
            sidekick.stop();
            sidekick
                .manager
                .finish_sidekick(sidekick.generation, sidekick.spawn_id);
            self.remember_stopped(id);
        }
        count
    }
}

impl<M: Model + Clone + 'static> Drop for SidekickRegistryInner<M> {
    fn drop(&mut self) {
        self.stop_all();
    }
}

struct Sidekick<M: Model + Clone + 'static> {
    agent: Agent<M>,
    context: Mutex<Context>,
    identity: Option<SubagentIdentity>,
    lifecycle: StdMutex<SidekickLifecycle>,
    limits: Limits,
    timeout: u32,
    manager: Arc<background::manager::SubagentManagerInner>,
    generation: u64,
    session_cancellation: CancellationToken,
    spawn_id: u64,
    spawn: SubagentSpawn,
    notifier: background::SubagentNotifier,
    _in_flight: InFlight,
}

enum SidekickLifecycle {
    Idle,
    Busy(CancellationToken),
    Stopped,
}

impl<M: Model + Clone + 'static> Sidekick<M> {
    fn status(&self) -> SidekickStatus {
        if self.session_cancellation.is_cancelled() {
            return SidekickStatus::Stopped;
        }
        match &*self.lifecycle.lock().unwrap() {
            SidekickLifecycle::Idle => SidekickStatus::Idle,
            SidekickLifecycle::Busy(_) => SidekickStatus::Busy,
            SidekickLifecycle::Stopped => SidekickStatus::Stopped,
        }
    }

    fn begin(
        &self,
        parent: CancellationToken,
    ) -> Result<(CancellationToken, tokio::task::JoinHandle<()>), ToolError> {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        if self.session_cancellation.is_cancelled() {
            *lifecycle = SidekickLifecycle::Stopped;
            return Err(ToolError::msg("sidekick is stopped"));
        }
        match &*lifecycle {
            SidekickLifecycle::Busy(_) => return Err(ToolError::msg("sidekick is busy")),
            SidekickLifecycle::Stopped => return Err(ToolError::msg("sidekick is stopped")),
            SidekickLifecycle::Idle => {}
        }
        let token = self.session_cancellation.child_token();
        let parent_token = token.clone();
        let watcher = tokio::spawn(async move {
            parent.cancelled().await;
            parent_token.cancel();
        });
        *lifecycle = SidekickLifecycle::Busy(token.clone());
        Ok((token, watcher))
    }

    fn finish(&self) {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        if matches!(*lifecycle, SidekickLifecycle::Busy(_))
            && self.manager.sidekick_live(self.generation, self.spawn_id)
        {
            *lifecycle = SidekickLifecycle::Idle;
        }
        self.manager.sidekick_idle(self.generation, self.spawn_id);
    }

    fn stop(&self) {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        if let SidekickLifecycle::Busy(token) = &*lifecycle {
            token.cancel();
        }
        *lifecycle = SidekickLifecycle::Stopped;
    }
}

struct TaskLease<M: Model + Clone + 'static> {
    sidekick: Arc<Sidekick<M>>,
    slot: Option<background::manager::Slot>,
    parent_watcher: tokio::task::JoinHandle<()>,
}

impl<M: Model + Clone + 'static> Drop for TaskLease<M> {
    fn drop(&mut self) {
        self.parent_watcher.abort();
        self.slot.take();
        self.sidekick.finish();
    }
}

struct InitialRegistration<M: Model + Clone + 'static> {
    registry: Arc<SidekickRegistryInner<M>>,
    sidekick: Arc<Sidekick<M>>,
    armed: bool,
}

impl<M: Model + Clone + 'static> Drop for InitialRegistration<M> {
    fn drop(&mut self) {
        if self.armed {
            self.registry
                .live
                .lock()
                .unwrap()
                .remove(&self.sidekick.spawn_id);
            self.sidekick.stop();
            self.sidekick
                .manager
                .finish_sidekick(self.sidekick.generation, self.sidekick.spawn_id);
        }
    }
}

/// One task for a spawned agent, as a host names it.
#[derive(Clone, Debug)]
pub struct SubagentRequest {
    pub(crate) task: String,
    pub(crate) system_prompt: Option<String>,
    pub(crate) model: Option<String>,
}

impl SubagentRequest {
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            system_prompt: None,
            model: None,
        }
    }

    /// Override the tool's default system prompt for this worker.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Request one host-approved model by id. Validated against the live
    /// `/subagents` route exactly like a model-supplied `model` argument.
    pub fn model(mut self, id: impl Into<String>) -> Self {
        self.model = Some(id.into());
        self
    }

    pub fn task(&self) -> &str {
        &self.task
    }

    pub fn system_prompt_override(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    pub fn requested_model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

/// A finished foreground worker: the typed form of the tool's JSON result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentOutcome {
    pub answer: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: u64,
    pub cache_create_tokens: u64,
    pub runtime_ms: u128,
    pub steps: u64,
    pub tool_calls: u64,
    pub timing: WorkerTiming,
    pub identity: Option<SubagentIdentity>,
}

/// Bounded worker latency samples. Tool durations are cumulative execution
/// time, so overlapping parallel calls may sum to more than wall-clock time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkerTiming {
    pub model_call_elapsed_ms: Vec<u128>,
    pub model_cumulative_ms: u128,
    pub tool_call_elapsed: Vec<ToolCallTiming>,
    pub tool_cumulative_ms: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallTiming {
    pub name: String,
    pub elapsed_ms: u128,
}

impl WorkerTiming {
    fn into_value(self) -> Value {
        json!({
            "modelCallElapsedMs": self.model_call_elapsed_ms,
            "modelCumulativeMs": self.model_cumulative_ms,
            "toolCallElapsed": self.tool_call_elapsed.into_iter().map(|call| json!({
                "name": call.name,
                "elapsedMs": call.elapsed_ms,
            })).collect::<Vec<_>>(),
            "toolCumulativeMs": self.tool_cumulative_ms,
        })
    }
}

impl SubagentOutcome {
    /// The JSON the `subagent` tool returns for a completed foreground run.
    pub fn into_value(self) -> Value {
        let timing = self.timing.into_value();
        json!({
            "answer": self.answer,
            "usage": Usage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                reasoning_tokens: self.reasoning_tokens,
                cache_read_tokens: self.cache_read_tokens,
                cache_create_tokens: self.cache_create_tokens,
            },
            "runtimeMs": self.runtime_ms,
            "steps": self.steps,
            "toolCalls": self.tool_calls,
            "timing": timing,
            "termination": "completed",
            "identity": self.identity,
        })
    }
}

/// An admitted detached worker: the typed form of the tool's acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundAcknowledgement {
    pub spawn_id: u64,
    pub status: BackgroundStatus,
    pub identity: Option<SubagentIdentity>,
}

impl BackgroundAcknowledgement {
    /// The JSON the `subagent` tool returns for a `background: true` call.
    pub fn into_value(self) -> Value {
        json!({
            "spawnId": self.spawn_id,
            "status": self.status.as_str(),
            "termination": "detached",
            "identity": self.identity,
        })
    }
}

impl<M: Model + Clone + 'static> SubagentTool<M> {
    fn prepare_sidekick(
        &self,
        request: SubagentRequest,
        completion: bool,
    ) -> Result<(Arc<Sidekick<M>>, String, Option<background::manager::Slot>), ToolError> {
        if self.background.is_none() {
            return Err(ToolError::msg(
                "persistent sidekicks require session-owned background subagents",
            ));
        }
        let prepared = self.prepare_request(request, None, false, None, true)?;
        let Prepared {
            agent,
            spawn,
            mut context,
            in_flight,
            mut limits,
            timeout,
            ..
        } = prepared;
        let id = spawn.id;
        let task = spawn.task.clone();
        context.push_system(SIDEKICK_GUIDANCE);
        // `prepare_spawn` intentionally did not admit a foreground run.
        // Register only after all fallible preparation/extensions complete,
        // atomically checking the manager's open generation and capacity.
        // Retain a reusable limits template: each task derives a new deadline.
        limits.deadline = self.limits.deadline;
        let manager = self.background.as_ref().unwrap().manager.inner.clone();
        let notifier = self.background.as_ref().unwrap().notifier.clone();
        let admission = manager
            .admit_sidekick(&spawn, completion)
            .map_err(ToolError::msg)?;
        let (generation, session_cancellation, slot) = admission.into_parts();
        let sidekick = Arc::new(Sidekick {
            agent,
            context: Mutex::new(context),
            identity: spawn.identity.clone(),
            lifecycle: StdMutex::new(SidekickLifecycle::Idle),
            limits,
            timeout,
            manager,
            generation,
            session_cancellation,
            spawn_id: id,
            spawn,
            notifier,
            _in_flight: in_flight,
        });
        self.sidekicks
            .inner
            .live
            .lock()
            .unwrap()
            .insert(id, sidekick.clone());
        Ok((sidekick, task, slot))
    }

    /// Start a persistent sidekick in the background and return immediately.
    pub fn start_sidekick(
        &self,
        request: SubagentRequest,
    ) -> Result<SidekickAcknowledgement, ToolError> {
        let (sidekick, task, slot) = self.prepare_sidekick(request, true)?;
        let status = if slot.is_some() {
            BackgroundStatus::Running
        } else {
            BackgroundStatus::Queued
        };
        let acknowledgement = SidekickAcknowledgement {
            spawn_id: sidekick.spawn_id,
            status,
            identity: sidekick.identity.clone(),
        };
        self.spawn_sidekick_task(sidekick, task, slot);
        Ok(acknowledgement)
    }

    /// Explicit foreground form retained for callers that need to await the
    /// initial report. A failed first turn removes its undisclosed handle.
    pub async fn start_sidekick_foreground(
        &self,
        request: SubagentRequest,
        cancellation: CancellationToken,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<SidekickReport, ToolError> {
        let (sidekick, task, slot) = self.prepare_sidekick(request, false)?;
        let mut registration = InitialRegistration {
            registry: self.sidekicks.inner.clone(),
            sidekick: sidekick.clone(),
            armed: true,
        };
        let (token, parent_watcher) = sidekick.begin(cancellation)?;
        let slot = match slot {
            Some(slot) => Some(slot),
            None => Some(
                sidekick
                    .manager
                    .acquire(sidekick.spawn_id, &token)
                    .await
                    .ok_or_else(|| ToolError::msg("sidekick is stopped"))?,
            ),
        };
        let lease = TaskLease {
            sidekick: sidekick.clone(),
            slot,
            parent_watcher,
        };
        let result = self
            .run_sidekick_task(sidekick.clone(), &task, token, deadline, lease)
            .await;
        if result.is_ok() {
            registration.armed = false;
        }
        result
    }

    /// Queue one background turn on an idle sidekick and return immediately.
    pub fn sidekick_task(
        &self,
        spawn_id: u64,
        task: &str,
    ) -> Result<SidekickAcknowledgement, ToolError> {
        let sidekick = self
            .sidekicks
            .inner
            .live
            .lock()
            .unwrap()
            .get(&spawn_id)
            .cloned()
            .ok_or_else(|| self.sidekick_lookup_error(spawn_id))?;
        if !sidekick
            .manager
            .sidekick_live(sidekick.generation, sidekick.spawn_id)
        {
            sidekick.stop();
            return Err(ToolError::msg("sidekick is stopped"));
        }
        // Claim Busy before admission so overlapping calls reject even while
        // this turn is queued behind the manager's bounded concurrency.
        let (token, parent_watcher) = sidekick.begin(CancellationToken::new())?;
        let slot = match sidekick
            .manager
            .queue_sidekick(sidekick.generation, spawn_id, true)
        {
            Ok(slot) => slot,
            Err(error) => {
                parent_watcher.abort();
                sidekick.finish();
                return Err(ToolError::msg(error));
            }
        };
        let status = if slot.is_some() {
            BackgroundStatus::Running
        } else {
            BackgroundStatus::Queued
        };
        let acknowledgement = SidekickAcknowledgement {
            spawn_id,
            status,
            identity: sidekick.identity.clone(),
        };
        self.spawn_admitted_sidekick_task(sidekick, task.to_string(), token, parent_watcher, slot);
        Ok(acknowledgement)
    }

    /// Explicit foreground follow-up.
    pub async fn sidekick_task_foreground(
        &self,
        spawn_id: u64,
        task: &str,
        cancellation: CancellationToken,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<SidekickReport, ToolError> {
        let sidekick = self
            .sidekicks
            .inner
            .live
            .lock()
            .unwrap()
            .get(&spawn_id)
            .cloned()
            .ok_or_else(|| self.sidekick_lookup_error(spawn_id))?;
        let (token, parent_watcher) = sidekick.begin(cancellation)?;
        let mut lease = TaskLease {
            sidekick: sidekick.clone(),
            slot: None,
            parent_watcher,
        };
        let slot = match sidekick
            .manager
            .queue_sidekick(sidekick.generation, spawn_id, false)
        {
            Ok(Some(slot)) => slot,
            Ok(None) => sidekick
                .manager
                .acquire(spawn_id, &token)
                .await
                .ok_or_else(|| ToolError::msg("sidekick is stopped"))?,
            Err(error) => return Err(ToolError::msg(error)),
        };
        lease.slot = Some(slot);
        self.run_sidekick_task(sidekick, task, token, deadline, lease)
            .await
    }

    fn spawn_sidekick_task(
        &self,
        sidekick: Arc<Sidekick<M>>,
        task: String,
        slot: Option<background::manager::Slot>,
    ) {
        let (token, parent_watcher) = sidekick
            .begin(CancellationToken::new())
            .expect("new sidekick is idle");
        self.spawn_admitted_sidekick_task(sidekick, task, token, parent_watcher, slot);
    }

    fn spawn_admitted_sidekick_task(
        &self,
        sidekick: Arc<Sidekick<M>>,
        task: String,
        token: CancellationToken,
        parent_watcher: tokio::task::JoinHandle<()>,
        slot: Option<background::manager::Slot>,
    ) {
        let tool = self.clone();
        tokio::spawn(async move {
            let slot = match slot {
                Some(slot) => Some(slot),
                None => sidekick.manager.acquire(sidekick.spawn_id, &token).await,
            };
            let result = match slot {
                Some(slot) => {
                    let lease = TaskLease {
                        sidekick: sidekick.clone(),
                        slot: Some(slot),
                        parent_watcher,
                    };
                    tool.run_sidekick_task(sidekick.clone(), &task, token, None, lease)
                        .await
                        .map(SidekickReport::into_value)
                        .map_err(|error| error.to_string())
                }
                None => {
                    parent_watcher.abort();
                    sidekick.finish();
                    Err("sidekick stopped before execution".to_string())
                }
            };
            if sidekick
                .manager
                .sidekick_live(sidekick.generation, sidekick.spawn_id)
            {
                let mut spawn = sidekick.spawn.clone();
                spawn.task = task;
                (sidekick.notifier)(SubagentNotification {
                    generation: sidekick.manager.generation(),
                    spawn,
                    result,
                });
            }
        });
    }

    async fn run_sidekick_task(
        &self,
        sidekick: Arc<Sidekick<M>>,
        task: &str,
        token: CancellationToken,
        deadline: Option<tokio::time::Instant>,
        _lease: TaskLease<M>,
    ) -> Result<SidekickReport, ToolError> {
        let spawn_id = sidekick.spawn_id;
        // Execute transactionally. Cancellation, errors, and dropping this
        // future discard the working transcript, including unmatched tool
        // calls; only a complete final response replaces retained context.
        let mut working = sidekick.context.lock().await.clone();
        working.push_user(task);
        let mut limits = sidekick.limits.clone();
        limits.deadline = execution_deadline(
            limits.deadline.into_iter().chain(deadline).min(),
            sidekick.timeout,
        );
        // `Agent` deliberately exposes no mutable per-run limits API. Its
        // retained sidekick instance therefore has no deadline; enforce this
        // task's derived deadline through its task cancellation token.
        if let Some(deadline) = limits.deadline {
            let timeout = token.clone();
            tokio::spawn(async move {
                tokio::time::sleep_until(deadline).await;
                timeout.cancel();
            });
        }
        let result = sidekick.agent.run_context(&mut working, token).await;
        let answer =
            result.map_err(|error| ToolError::msg(format!("sidekick task failed: {error}")))?;
        *sidekick.context.lock().await = working;
        Ok(SidekickReport {
            spawn_id,
            status: SidekickStatus::Idle,
            answer,
            identity: sidekick.identity.clone(),
        })
    }

    pub fn sidekick_status(&self, spawn_id: u64) -> Result<SidekickStatus, ToolError> {
        if let Some(sidekick) = self
            .sidekicks
            .inner
            .live
            .lock()
            .unwrap()
            .get(&spawn_id)
            .cloned()
        {
            if !sidekick
                .manager
                .sidekick_live(sidekick.generation, sidekick.spawn_id)
            {
                sidekick.stop();
            }
            return Ok(sidekick.status());
        }
        if self
            .sidekicks
            .inner
            .stopped
            .lock()
            .unwrap()
            .contains(&spawn_id)
        {
            Ok(SidekickStatus::Stopped)
        } else {
            Err(ToolError::msg("unknown sidekick handle"))
        }
    }

    pub fn stop_sidekick(&self, spawn_id: u64) -> Result<(), ToolError> {
        if self
            .sidekicks
            .inner
            .stopped
            .lock()
            .unwrap()
            .contains(&spawn_id)
        {
            return Ok(());
        }
        let sidekick = self
            .sidekicks
            .inner
            .live
            .lock()
            .unwrap()
            .remove(&spawn_id)
            .ok_or_else(|| ToolError::msg("unknown sidekick handle"))?;
        sidekick.stop();
        sidekick
            .manager
            .finish_sidekick(sidekick.generation, spawn_id);
        self.sidekicks.inner.remember_stopped(spawn_id);
        Ok(())
    }

    pub fn stop_all_sidekicks(&self) -> usize {
        self.sidekicks.inner.stop_all()
    }

    fn sidekick_lookup_error(&self, spawn_id: u64) -> ToolError {
        if self
            .sidekicks
            .inner
            .stopped
            .lock()
            .unwrap()
            .contains(&spawn_id)
        {
            ToolError::msg("sidekick is stopped")
        } else {
            ToolError::msg("unknown sidekick handle")
        }
    }

    /// Whether detached (`background`) execution is configured here.
    pub fn has_background(&self) -> bool {
        self.background.is_some()
    }

    /// Run one worker to completion in the foreground.
    ///
    /// Host-originated spawns are subject to the same routing validation,
    /// admission limits, and spawn extensions as model-originated ones.
    /// `cancellation` is honoured as-is (the tool path derives a child of
    /// the calling turn's token); `deadline` bounds the run together with
    /// the configured limits and the live worker timeout.
    pub async fn run_foreground(
        &self,
        request: SubagentRequest,
        cancellation: CancellationToken,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<SubagentOutcome, ToolError> {
        let prepared = self.prepare_request(request, None, false, deadline, false)?;
        Self::execute_foreground(prepared, cancellation).await
    }

    /// Admit one detached worker and return its acknowledgement at once.
    /// The result reaches the notifier given to [`Self::background`].
    ///
    /// Host-originated spawns are subject to the same routing validation,
    /// admission limits, and spawn extensions as model-originated ones.
    /// Fails, with the tool's message, when background execution is not
    /// configured at this depth.
    pub fn spawn_background(
        &self,
        request: SubagentRequest,
    ) -> Result<BackgroundAcknowledgement, ToolError> {
        Ok(self
            .prepare_request(request, None, true, None, false)?
            .detach())
    }

    /// Admitted detached workers, running and queued, in spawn order.
    /// Empty when background execution is not configured.
    ///
    /// Unlike the tool's `action=list`, this never refuses a repeat with
    /// "no change since the previous list": that refusal keeps the model
    /// from polling, whereas a host may poll this freely.
    pub fn active_jobs(&self) -> Vec<BackgroundJob> {
        self.background
            .as_ref()
            .map(|config| config.manager.active())
            .unwrap_or_default()
    }

    /// Cancel one detached worker; `false` when it is unknown or already
    /// finished, or when background execution is not configured.
    pub fn cancel_job(&self, spawn_id: u64) -> bool {
        self.background
            .as_ref()
            .is_some_and(|config| config.manager.cancel(spawn_id))
    }

    /// Cancel every detached worker and report how many were admitted.
    pub fn cancel_all_jobs(&self) -> usize {
        self.background
            .as_ref()
            .map_or(0, |config| config.manager.cancel_all())
    }

    /// The single request path shared by the model tool and the typed
    /// API. `call_id` is `None` for host-originated spawns, which get the
    /// synthetic `host:<spawn id>` id so spawn extensions can tell them
    /// from model tool calls.
    pub(super) fn prepare_request(
        &self,
        request: SubagentRequest,
        call_id: Option<String>,
        detached: bool,
        deadline: Option<tokio::time::Instant>,
        retained_context: bool,
    ) -> Result<Prepared<M>, ToolError> {
        if detached && self.background.is_none() {
            return Err(ToolError::msg(
                "background subagents are unavailable in this host or nesting depth",
            ));
        }
        let id = self.next_spawn_id();
        let req = SpawnRequest {
            id,
            generation: None,
            expected_model_key: None,
            depth: None,
            task: request.task,
            system_prompt: request.system_prompt,
            model: request.model,
            call_id: call_id.unwrap_or_else(|| format!("host:{id}")),
            run: None,
            stage: None,
            parent_id: None,
            notifier: None,
        };
        self.prepare_spawn(
            req,
            detached,
            if detached { None } else { deadline },
            retained_context,
        )
    }

    /// Drive a prepared foreground worker and account for it however it ends.
    pub(super) async fn execute_foreground(
        prepared: Prepared<M>,
        cancellation: CancellationToken,
    ) -> Result<SubagentOutcome, ToolError> {
        let Prepared {
            agent,
            spawn,
            telemetry,
            started,
            in_flight,
            ..
        } = prepared;
        let _in_flight = in_flight;
        let result = agent.run_with_cancellation(&spawn.task, cancellation).await;
        subagent_outcome(result, &telemetry, started, spawn.identity.as_ref())
            .map_err(ToolError::msg)
    }
}
