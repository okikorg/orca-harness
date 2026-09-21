use super::host::WorkflowAcknowledgement;
use super::outcome::{StageTiming, WorkflowOutcome};
use super::store::WorkflowStore;
use crate::subagent::spawn::SpawnRequest;
use crate::{SubagentNotification, SubagentSpawn, SubagentTool};
use orca_harness_core::{Model, ToolError};
use orca_harness_dag::{Advance, Dag, Kind, RunId, RunOutcome, RunState, Stage, StageId};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
mod inspect;
struct Run {
    dag: Dag,
    spawn: SubagentSpawn,
    generation: u64,
    live: HashMap<u64, StageId>,
    spawns: HashMap<StageId, u64>,
    depths: HashMap<u64, u32>,
    model_keys: BTreeMap<Option<String>, Value>,
    persisted: HashSet<StageId>,
    started: Instant,
    timings: BTreeMap<StageId, StageTiming>,
    peak: usize,
    peak_running: Option<usize>,
    resume: Option<u64>,
    deadline: Option<tokio::time::Instant>,
}
/// What one emitted stage needs from the runtime: a replayed output that
/// settles at once, or a worker to admit.
enum SpawnPlan {
    Cached {
        advance: Advance,
        notification: SubagentNotification,
    },
    Live {
        request: SpawnRequest,
        deadline: Option<tokio::time::Instant>,
    },
}
enum HostEvent {
    Announce(SubagentSpawn),
    Notify(SubagentNotification),
    Spawn {
        run_id: u64,
        spawn_id: u64,
        stage: StageId,
        request: SpawnRequest,
        deadline: Option<tokio::time::Instant>,
    },
}
/// Lock order: `dispatch` > `runs` > the store; manager state is always
/// taken alone, never while `runs` is held.
pub(super) struct Runtime<M: Model + Clone + 'static> {
    pub subagent: Arc<SubagentTool<M>>,
    // Dispatch serializes state transition + its admissions with cancellation.
    // The DAG mutex is always dropped before touching the manager or host hooks.
    dispatch: Mutex<()>,
    runs: Mutex<HashMap<RunId, Run>>,
    store: WorkflowStore,
}
impl<M: Model + Clone + 'static> Runtime<M> {
    pub fn new(subagent: Arc<SubagentTool<M>>, store: WorkflowStore) -> Self {
        Self {
            subagent,
            dispatch: Mutex::new(()),
            runs: Mutex::new(HashMap::new()),
            store,
        }
    }
    /// Admit a validated graph. Callers validate the graph and every stage
    /// model first (see `WorkflowTool::admit`); this only checks the reuse
    /// source and the deadline before admission.
    pub fn submit(
        self: &Arc<Self>,
        dag: Dag,
        call_id: String,
        resume: Option<RunId>,
        timeout: Option<Duration>,
    ) -> Result<WorkflowAcknowledgement, ToolError> {
        let _dispatch = self.dispatch.lock().unwrap();
        if resume.is_some_and(|id| !self.store.exists(id)) {
            return Err(ToolError::msg("resumeFrom run does not exist"));
        }
        let id = self.subagent.next_spawn_id();
        let count = dag.stages().count();
        let spawn = SubagentSpawn {
            id,
            parent_id: None,
            depth: 0,
            call_id,
            task: format!("workflow · {count} stages"),
            identity: None,
            run: None,
            stage: None,
        };
        let requested_deadline = timeout
            .map(|timeout| {
                tokio::time::Instant::now()
                    .checked_add(timeout)
                    .ok_or_else(|| ToolError::msg("timeoutSeconds is too large"))
            })
            .transpose()?;
        let deadline = requested_deadline
            .into_iter()
            .chain(self.subagent.workflow_deadline())
            .min();
        let config = self.subagent.background_config().unwrap();
        let weak = Arc::downgrade(self);
        let admission = config
            .manager
            .inner
            .admit_run(
                &spawn,
                Arc::new(move |peak_running| {
                    if let Some(runtime) = weak.upgrade() {
                        let _ = runtime.cancel_local(id, Some(peak_running));
                    }
                }),
            )
            .map_err(ToolError::msg)?;
        self.store.create(id);
        let (generation, _, slot) = admission.into_parts();
        debug_assert!(slot.is_none());
        let model_keys = dag
            .stages()
            .map(|stage| {
                (
                    stage.model.clone(),
                    self.subagent.replay_model_key(stage.model.as_deref()),
                )
            })
            .collect();
        let mut run = Run {
            dag,
            spawn,
            generation,
            live: HashMap::new(),
            spawns: HashMap::new(),
            depths: HashMap::new(),
            model_keys,
            persisted: HashSet::new(),
            started: Instant::now(),
            timings: BTreeMap::new(),
            peak: 0,
            peak_running: None,
            resume,
            deadline,
        };
        let advance = run.dag.start();
        let root_spawn = run.spawn.clone();
        self.runs.lock().unwrap().insert(id, run);
        let events = self.apply(id, advance, vec![HostEvent::Announce(root_spawn)]);
        drop(_dispatch);
        self.deliver(events);
        Ok(WorkflowAcknowledgement {
            run_id: id,
            stages: count,
        })
    }
    fn key(&self, run: &Run, id: &StageId) -> String {
        let mut material: Value =
            serde_json::from_str(&run.dag.stage_key(id)).expect("canonical key");
        for stage in material.as_array_mut().unwrap() {
            stage["model"] = run.model_keys[&stage["model"].as_str().map(str::to_owned)].clone();
        }
        material.to_string()
    }
    /// Spill every newly completed output, including virtual map barriers.
    fn persist(&self, run: &mut Run) {
        let ids: Vec<_> = run
            .dag
            .stages()
            .filter(|stage| {
                !run.persisted.contains(&stage.id) && run.dag.output(&stage.id).is_some()
            })
            .map(|stage| stage.id.clone())
            .collect();
        for id in ids {
            self.store.write(
                run.spawn.id,
                &id,
                self.key(run, &id),
                run.dag.output(&id).unwrap().into(),
            );
            run.persisted.insert(id);
        }
    }
    pub fn cancel(self: &Arc<Self>, id: u64) -> Result<(), ToolError> {
        let manager = &self.subagent.background_config().unwrap().manager;
        if !manager.inner.run_ids().contains(&id) || !manager.cancel(id) {
            return Err(ToolError::msg("workflow is not running"));
        }
        Ok(())
    }
    fn cancel_local(
        self: &Arc<Self>,
        id: u64,
        peak_running: Option<usize>,
    ) -> Result<(), ToolError> {
        let _dispatch = self.dispatch.lock().unwrap();
        let advance = {
            let mut runs = self.runs.lock().unwrap();
            let run = runs
                .get_mut(&id)
                .ok_or_else(|| ToolError::msg("workflow is not running"))?;
            if let Some(peak_running) = peak_running {
                run.peak_running = Some(peak_running);
            }
            run.dag.cancel()
        };
        let events = self.apply(id, advance, Vec::new());
        drop(_dispatch);
        self.deliver(events);
        Ok(())
    }
    fn complete(self: &Arc<Self>, notification: SubagentNotification) -> Vec<HostEvent> {
        let _dispatch = self.dispatch.lock().unwrap();
        let id = notification.spawn.run.unwrap();
        let config = self.subagent.background_config().unwrap();
        if !config.manager.is_current(notification.generation) {
            return Vec::new();
        }
        let advance = {
            let mut runs = self.runs.lock().unwrap();
            let Some(run) = runs.get_mut(&id) else {
                return Vec::new();
            };
            let Some(stage) = run.live.remove(&notification.spawn.id) else {
                return Vec::new();
            };
            run.timings.insert(
                stage.clone(),
                StageTiming::Ran {
                    runtime_ms: notification
                        .result
                        .as_ref()
                        .ok()
                        .and_then(|v| v.get("runtimeMs"))
                        .and_then(Value::as_u64),
                    cached: false,
                },
            );
            let result = notification.result.and_then(|v| {
                v.get("answer")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| "stage returned no answer".into())
            });
            let advance = run.dag.complete(&stage, result);
            self.persist(run);
            advance
        };
        self.apply(id, advance, Vec::new())
    }
    fn fail(&self, id: u64, error: String, events: &mut Vec<HostEvent>) {
        if let Some(notification) = self.finish(
            id,
            RunOutcome {
                state: RunState::Failed,
                outputs: BTreeMap::new(),
                stages: BTreeMap::new(),
                degraded: false,
                error: Some(error),
            },
        ) {
            events.push(HostEvent::Notify(notification));
        }
    }
    /// Book one emitted stage under `runs`: a replayed output completes
    /// the stage there and then; a live one is registered before its
    /// worker is prepared. `None` when the run is gone or no longer
    /// running, in which case nothing was booked.
    fn plan_stage(self: &Arc<Self>, id: u64, spawn_id: u64, stage: Stage) -> Option<SpawnPlan> {
        let mut runs = self.runs.lock().unwrap();
        let run = runs.get_mut(&id)?;
        if run.dag.state != RunState::Running {
            return None;
        }
        let key = self.key(run, &stage.id);
        let cached = run
            .resume
            .and_then(|prior| self.store.replay(prior, &stage.id, &key));
        let parent = run
            .dag
            .map_source(&stage.id)
            .and_then(|source| run.spawns.get(source))
            .copied()
            .unwrap_or(id);
        let depth = run.depths.get(&parent).copied().unwrap_or(0) + 1;
        run.depths.insert(spawn_id, depth);
        run.spawns.insert(stage.id.clone(), spawn_id);
        if let Some(answer) = cached {
            run.timings.insert(
                stage.id.clone(),
                StageTiming::Ran {
                    runtime_ms: Some(0),
                    cached: true,
                },
            );
            let advance = run.dag.complete(&stage.id, Ok(answer.clone()));
            self.persist(run);
            return Some(SpawnPlan::Cached {
                advance,
                notification: SubagentNotification {
                    generation: run.generation,
                    spawn: SubagentSpawn {
                        id: spawn_id,
                        parent_id: Some(parent),
                        depth,
                        call_id: run.spawn.call_id.clone(),
                        task: stage.prompt,
                        identity: None,
                        run: Some(id),
                        stage: Some(stage.id),
                    },
                    result: Ok(json!({"answer":answer,"runtimeMs":0,"cached":true})),
                },
            });
        }
        run.live.insert(spawn_id, stage.id.clone());
        run.peak = run.peak.max(run.live.len());
        let runtime = self.clone();
        let request = SpawnRequest {
            id: spawn_id,
            generation: Some(run.generation),
            expected_model_key: Some(run.model_keys[&stage.model].clone()),
            depth: Some(depth),
            task: stage.prompt,
            system_prompt: None,
            model: stage.model,
            call_id: run.spawn.call_id.clone(),
            run: Some(id),
            stage: Some(stage.id),
            parent_id: Some(parent),
            notifier: Some(Arc::new(move |notification| {
                // Advance the DAG before entering host code: a blocking or
                // panicking callback must not strand dependent stages.
                let stage_notification = notification.clone();
                let events = runtime.complete(notification);
                let (internal, external): (Vec<_>, Vec<_>) = events
                    .into_iter()
                    .partition(|event| matches!(event, HostEvent::Spawn { .. }));
                runtime.deliver(internal);
                let notified = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (runtime.subagent.background_config().unwrap().notifier)(stage_notification);
                }));
                runtime.deliver(external);
                if let Err(panic) = notified {
                    std::panic::resume_unwind(panic);
                }
            })),
        };
        Some(SpawnPlan::Live {
            request,
            deadline: run.deadline,
        })
    }
    /// A stage whose worker could not be prepared fails the run; `None`
    /// when the run is gone.
    fn abandon_stage(
        &self,
        id: u64,
        spawn_id: u64,
        stage: &StageId,
        error: String,
    ) -> Option<Advance> {
        let mut runs = self.runs.lock().unwrap();
        let run = runs.get_mut(&id)?;
        run.live.remove(&spawn_id);
        Some(run.dag.complete(stage, Err(error)))
    }

    fn fail_stage(self: &Arc<Self>, id: u64, spawn_id: u64, stage: StageId, error: String) {
        let _dispatch = self.dispatch.lock().unwrap();
        let Some(advance) = self.abandon_stage(id, spawn_id, &stage, error) else {
            return;
        };
        let events = self.apply(id, advance, Vec::new());
        drop(_dispatch);
        self.deliver(events);
    }
    fn apply(
        self: &Arc<Self>,
        id: u64,
        advance: Advance,
        mut events: Vec<HostEvent>,
    ) -> Vec<HostEvent> {
        let mut work = VecDeque::from([advance]);
        while let Some(advance) = work.pop_front() {
            match advance {
                Advance::Spawn(stages) => {
                    for stage in stages {
                        let spawn_id = self.subagent.next_spawn_id();
                        let stage_id = stage.id.clone();
                        let Some(plan) = self.plan_stage(id, spawn_id, stage) else {
                            break;
                        };
                        match plan {
                            SpawnPlan::Cached {
                                advance,
                                notification,
                            } => {
                                events.push(HostEvent::Announce(notification.spawn.clone()));
                                events.push(HostEvent::Notify(notification));
                                work.push_back(advance);
                            }
                            SpawnPlan::Live { request, deadline } => {
                                events.push(HostEvent::Spawn {
                                    run_id: id,
                                    spawn_id,
                                    stage: stage_id,
                                    request,
                                    deadline,
                                });
                            }
                        }
                    }
                }
                Advance::Done(outcome) => {
                    if let Some(notification) = self.finish(id, outcome) {
                        events.push(HostEvent::Notify(notification));
                    }
                    return events;
                }
                Advance::Stalled(error) => {
                    self.fail(id, error, &mut events);
                    return events;
                }
            }
        }
        events
    }

    fn deliver(self: &Arc<Self>, events: Vec<HostEvent>) {
        for event in events {
            match event {
                HostEvent::Announce(spawn) => self.subagent.announce(&spawn),
                HostEvent::Notify(notification) => {
                    (self.subagent.background_config().unwrap().notifier)(notification)
                }
                HostEvent::Spawn {
                    run_id,
                    spawn_id,
                    stage,
                    request,
                    deadline,
                } => match self.subagent.prepare_spawn(request, true, deadline, false) {
                    Ok(prepared) => {
                        prepared.detach();
                    }
                    Err(error) => self.fail_stage(run_id, spawn_id, stage, error.to_string()),
                },
            }
        }
    }
    /// Record the terminal outcome, release the run, and notify the host
    /// once at run level: the outcome as JSON under `workflow` (and as the
    /// `answer` string), or, for a run that did not finish `Done`, an
    /// error that leads with the cause and carries the same JSON.
    fn finish(&self, id: u64, outcome: RunOutcome) -> Option<SubagentNotification> {
        let Some(mut run) = self.runs.lock().unwrap().remove(&id) else {
            return None;
        };
        let config = self.subagent.background_config().unwrap();
        for spawn in run.live.keys() {
            config.manager.cancel(*spawn);
        }
        let failed = outcome.state != RunState::Done;
        let runtime_ms = u64::try_from(run.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut outcome = WorkflowOutcome::new(outcome, runtime_ms);
        outcome.peak_admitted = run.peak;
        outcome.peak_running = run
            .peak_running
            .unwrap_or_else(|| config.manager.inner.run_peak(id));
        for stage in run.dag.stages() {
            run.timings
                .entry(stage.id.clone())
                .or_insert_with(|| StageTiming::Skipped {
                    virtual_stage: stage.kind == Kind::Map,
                });
        }
        outcome.timings = std::mem::take(&mut run.timings);
        self.persist(&mut run);
        self.store.set_outcome(id, &outcome);
        config.manager.inner.finish(run.generation, id);
        let value = serde_json::to_value(&outcome).expect("an outcome serializes");
        let answer = value.to_string();
        Some(SubagentNotification {
            generation: run.generation,
            spawn: run.spawn,
            result: if failed {
                // Lead with the cause: the per-stage map cannot name it, and a
                // reader that stops at the first line must still see why.
                Err(format!(
                    "workflow {} ({}): {}\n{answer}",
                    value["state"].as_str().unwrap_or("failed"),
                    id,
                    outcome.error.as_deref().unwrap_or("no error reported"),
                ))
            } else {
                Ok(json!({"answer":answer,"workflow":value,"termination":"completed"}))
            },
        })
    }
}
