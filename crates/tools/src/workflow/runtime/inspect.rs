//! Read-only views over the runtime: every live run, one run live or
//! finished, and one stage's stored output.
use super::Runtime;
use crate::workflow::host::{StageOutput, WorkflowStageJob, WorkflowStatus};
use orca_harness_core::{Model, ToolError};
use orca_harness_dag::{RunId, RunState};
use std::collections::BTreeMap;

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
                let (state, stages) = runs
                    .get(&id)
                    .map(|run| (run.dag.state.clone(), run.dag.statuses().clone()))
                    .unwrap_or((RunState::Running, BTreeMap::new()));
                WorkflowStatus {
                    run_id: id,
                    state,
                    stages,
                    active: jobs
                        .iter()
                        .filter(|job| job.spawn.run == Some(id))
                        .map(|job| WorkflowStageJob {
                            spawn_id: job.spawn.id,
                            stage: job.spawn.stage.clone(),
                            status: job.status,
                        })
                        .collect(),
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
        let active = manager
            .active()
            .into_iter()
            .filter(|job| job.spawn.run == Some(id))
            .map(|job| WorkflowStageJob {
                spawn_id: job.spawn.id,
                stage: job.spawn.stage,
                status: job.status,
            })
            .collect();
        let (state, stages) = self
            .runs
            .lock()
            .unwrap()
            .get(&id)
            .map(|run| (run.dag.state.clone(), run.dag.statuses().clone()))
            .unwrap_or((RunState::Running, BTreeMap::new()));
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
