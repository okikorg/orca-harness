use super::store::WorkflowStore;
use crate::subagent::spawn::SpawnRequest;
use crate::{SubagentNotification, SubagentSpawn, SubagentTool};
use orca_harness_core::{Model, ToolContext, ToolError};
use orca_harness_dag::{Advance, Dag, RunId, RunState, StageId};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;
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
    timings: BTreeMap<StageId, Value>,
    peak: usize,
    resume: Option<u64>,
    deadline: Option<tokio::time::Instant>,
}
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
    pub fn submit(
        self: &Arc<Self>,
        dag: Dag,
        ctx: &ToolContext,
        resume: Option<u64>,
        timeout: Option<u64>,
    ) -> Result<Value, ToolError> {
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
            call_id: ctx.call_id.clone(),
            task: format!("workflow · {count} stages"),
            identity: None,
            run: None,
            stage: None,
        };
        let requested_deadline = timeout
            .map(|seconds| {
                tokio::time::Instant::now()
                    .checked_add(std::time::Duration::from_secs(seconds))
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
                Arc::new(move || {
                    if let Some(runtime) = weak.upgrade() {
                        let _ = runtime.cancel_local(id);
                    }
                }),
            )
            .map_err(ToolError::msg)?;
        self.store.create(id);
        self.subagent.announce(&spawn);
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
            resume,
            deadline,
        };
        let advance = run.dag.start();
        self.runs.lock().unwrap().insert(id, run);
        self.apply(id, advance);
        Ok(json!({"runId":id,"status":"running","stages":count,"termination":"detached"}))
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
    pub fn list(&self) -> Value {
        let manager = &self.subagent.background_config().unwrap().manager;
        let jobs = manager.active();
        let ids = manager.inner.run_ids();
        json!({"runs":ids.iter().map(|id| json!({"runId":id,"state":"running","activeStages":jobs.iter().filter(|job| job.spawn.run == Some(*id)).map(|job| json!({"spawnId":job.spawn.id,"status":job.status.as_str()})).collect::<Vec<_>>()})).collect::<Vec<_>>()})
    }

    pub fn output(&self, id: u64, stage: &str) -> Result<Value, ToolError> {
        self.store
            .output(id, stage)
            .map(|answer| json!({"runId":id,"stage":stage,"answer":answer}))
            .ok_or_else(|| ToolError::msg("stage output is unavailable"))
    }
    pub fn cancel(self: &Arc<Self>, id: u64) -> Result<(), ToolError> {
        let manager = &self.subagent.background_config().unwrap().manager;
        if !manager.inner.run_ids().contains(&id) || !manager.cancel(id) {
            return Err(ToolError::msg("workflow is not running"));
        }
        Ok(())
    }
    fn cancel_local(self: &Arc<Self>, id: u64) -> Result<(), ToolError> {
        let _dispatch = self.dispatch.lock().unwrap();
        let advance = {
            let mut runs = self.runs.lock().unwrap();
            let run = runs
                .get_mut(&id)
                .ok_or_else(|| ToolError::msg("workflow is not running"))?;
            run.dag.cancel()
        };
        self.apply(id, advance);
        Ok(())
    }
    fn complete(self: &Arc<Self>, notification: SubagentNotification) {
        let _dispatch = self.dispatch.lock().unwrap();
        let id = notification.spawn.run.unwrap();
        let config = self.subagent.background_config().unwrap();
        if !config.manager.is_current(notification.generation) {
            return;
        }
        let advance = {
            let mut runs = self.runs.lock().unwrap();
            let Some(run) = runs.get_mut(&id) else {
                return;
            };
            let Some(stage) = run.live.remove(&notification.spawn.id) else {
                return;
            };
            run.timings.insert(stage.clone(), json!({"runtimeMs":notification.result.as_ref().ok().and_then(|v| v.get("runtimeMs")).cloned(),"cached":false}));
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
        self.apply(id, advance);
    }
    fn fail(self: &Arc<Self>, id: u64, error: String) {
        self.finish(id, json!({"state":"failed","error":error}), true);
    }
    fn apply(self: &Arc<Self>, id: u64, advance: Advance) {
        let mut work = VecDeque::from([advance]);
        while let Some(advance) = work.pop_front() {
            match advance {
                Advance::Spawn(stages) => {
                    for stage in stages {
                        let spawn_id = self.subagent.next_spawn_id();
                        let (request, cached) = {
                            let mut runs = self.runs.lock().unwrap();
                            let Some(run) = runs.get_mut(&id) else {
                                return;
                            };
                            if run.dag.state != RunState::Running {
                                break;
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
                                run.timings
                                    .insert(stage.id.clone(), json!({"runtimeMs":0,"cached":true}));
                                let advance = run.dag.complete(&stage.id, Ok(answer.clone()));
                                self.persist(run);
                                (
                                    None,
                                    Some((
                                        advance,
                                        SubagentNotification {
                                            generation: run.generation,
                                            spawn: SubagentSpawn {
                                                id: spawn_id,
                                                parent_id: Some(parent),
                                                depth,
                                                call_id: run.spawn.call_id.clone(),
                                                task: stage.prompt.clone(),
                                                identity: None,
                                                run: Some(id),
                                                stage: Some(stage.id.clone()),
                                            },
                                            result: Ok(
                                                json!({"answer":answer,"runtimeMs":0,"cached":true}),
                                            ),
                                        },
                                    )),
                                )
                            } else {
                                run.live.insert(spawn_id, stage.id.clone());
                                run.spawns.insert(stage.id.clone(), spawn_id);
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
                                    stage: Some(stage.id.clone()),
                                    parent_id: Some(parent),
                                    notifier: Some(Arc::new(move |notification| {
                                        // Host settles the same existing UI row; stage results are not inbox entries.
                                        (runtime.subagent.background_config().unwrap().notifier)(
                                            notification.clone(),
                                        );
                                        runtime.complete(notification);
                                    })),
                                };
                                (Some((request, run.deadline)), None)
                            }
                        };
                        if let Some((cached, notification)) = cached {
                            self.subagent.announce(&notification.spawn);
                            (self.subagent.background_config().unwrap().notifier)(notification);
                            work.push_back(cached);
                            continue;
                        }
                        if let Some((request, deadline)) = request {
                            match self.subagent.prepare_spawn(request, true, deadline) {
                                Ok(prepared) => {
                                    prepared.detach();
                                }
                                Err(error) => {
                                    let advance = {
                                        let mut runs = self.runs.lock().unwrap();
                                        let Some(run) = runs.get_mut(&id) else {
                                            return;
                                        };
                                        run.live.remove(&spawn_id);
                                        run.dag.complete(&stage.id, Err(error.to_string()))
                                    };
                                    work.push_front(advance);
                                    break;
                                }
                            }
                        }
                    }
                }
                Advance::Done(outcome) => {
                    let failed = outcome.state != RunState::Done;
                    self.finish(id, serde_json::to_value(outcome).unwrap(), failed);
                    return;
                }
                Advance::Stalled(error) => {
                    self.fail(id, error);
                    return;
                }
            }
        }
    }
    fn finish(&self, id: u64, mut outcome: Value, failed: bool) {
        let Some(mut run) = self.runs.lock().unwrap().remove(&id) else {
            return;
        };
        let config = self.subagent.background_config().unwrap();
        for spawn in run.live.keys() {
            config.manager.cancel(*spawn);
        }
        outcome["runtimeMs"] = json!(run.started.elapsed().as_millis());
        outcome["peakAdmitted"] = json!(run.peak);
        outcome["peakRunning"] = json!(config.manager.inner.run_peak(id));
        for stage in run.dag.stages() {
            run.timings.entry(stage.id.clone()).or_insert_with(
                || json!({"runtimeMs":null,"virtual":stage.kind == orca_harness_dag::Kind::Map}),
            );
        }
        outcome["timings"] = json!(run.timings);
        self.persist(&mut run);
        self.store.outcome(id, &outcome);
        config.manager.inner.finish(run.generation, id);
        let answer = outcome.to_string();
        (config.notifier)(SubagentNotification {
            generation: run.generation,
            spawn: run.spawn,
            result: if failed {
                // Lead with the cause: the per-stage map cannot name it, and a
                // reader that stops at the first line must still see why.
                Err(format!(
                    "workflow {} ({}): {}\n{answer}",
                    outcome["state"].as_str().unwrap_or("failed"),
                    id,
                    outcome["error"].as_str().unwrap_or("no error reported"),
                ))
            } else {
                Ok(json!({"answer":answer,"workflow":outcome,"termination":"completed"}))
            },
        });
    }
}
