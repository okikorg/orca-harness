//! Read-only views over the runtime: every live run, one run live or
//! finished, and one stage's stored output.
use super::{Run, Runtime};
use crate::workflow::host::{StageOutput, WorkflowStageJob, WorkflowStatus};
use crate::BackgroundJob;
use orca_harness_core::{Model, ToolError};
use orca_harness_dag::{RunId, RunState, StageId, StageStatus};
use std::collections::BTreeMap;

/// The stage workers of `run` among the manager's jobs.
fn stage_jobs(jobs: &[BackgroundJob], run: RunId) -> Vec<WorkflowStageJob> {
    jobs.iter()
        .filter(|job| job.spawn.run == Some(run))
        .map(|job| WorkflowStageJob {
            spawn_id: job.spawn.id,
            stage: job.spawn.stage.clone(),
            status: job.status,
        })
        .collect()
}

/// A live run's state and per-stage statuses from its DAG; a run this
/// runtime does not own (admitted by another tool over the same manager)
/// reads as `Running` with none.
fn live_state(run: Option<&Run>) -> (RunState, BTreeMap<StageId, StageStatus>) {
    run.map(|run| (run.dag.state.clone(), run.dag.statuses().clone()))
        .unwrap_or((RunState::Running, BTreeMap::new()))
}

impl<M: Model + Clone + 'static> Runtime<M> {
    /// Every run the manager still holds, in id order. Per-stage statuses
    /// come from this runtime's DAG when it owns the run; a run admitted by
    /// another tool over the same manager reports `Running` with none.
    pub fn statuses(&self) -> Vec<WorkflowStatus> {
        let manager = &self.subagent.background_config().unwrap().manager;
        let jobs = manager.active();
        let ids = manager.inner.run_ids();
        let runs = self.runs.lock().unwrap();
        ids.into_iter()
            .map(|id| {
                let (state, stages) = live_state(runs.get(&id));
                WorkflowStatus {
                    run_id: id,
                    state,
                    stages,
                    active: stage_jobs(&jobs, id),
                    outcome: None,
                }
            })
            .collect()
    }

    /// A finished run's status from the outcome this runtime recorded in
    /// the store before delivering it, else a live run's. The store is
    /// consulted first: `finish` records the outcome before the manager
    /// forgets the run, so a run caught in that window reads as finished
    /// rather than as live with no stages.
    pub fn status(&self, id: RunId) -> Option<WorkflowStatus> {
        if let Some(outcome) = self.store.stored_outcome(id) {
            return Some(WorkflowStatus {
                run_id: id,
                state: outcome.state.clone(),
                stages: outcome.stages.clone(),
                active: Vec::new(),
                outcome: Some(outcome),
            });
        }
        let manager = &self.subagent.background_config().unwrap().manager;
        if !manager.inner.run_ids().contains(&id) {
            return None;
        }
        let active = stage_jobs(&manager.active(), id);
        let (state, stages) = live_state(self.runs.lock().unwrap().get(&id));
        Some(WorkflowStatus {
            run_id: id,
            state,
            stages,
            active,
            outcome: None,
        })
    }

    pub fn output(&self, id: RunId, stage: &str) -> Result<StageOutput, ToolError> {
        self.store
            .output(id, stage)
            .map(|answer| StageOutput {
                run_id: id,
                stage: stage.to_string(),
                answer,
            })
            .ok_or_else(|| ToolError::msg("stage output is unavailable"))
    }
}
