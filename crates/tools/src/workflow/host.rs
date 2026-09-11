//! Typed host operations over the same implementation the `workflow`
//! model tool runs.
//!
//! A host (the SDK, an embedding application) submits, inspects, reads,
//! and cancels workflow runs without composing tool-call JSON or a
//! `ToolContext`. Every entry point here and
//! [`Tool::call`](orca_harness_core::Tool::call) funnel through one
//! private path: [`WorkflowSubmission`] -> `harness-dag` graph validation
//! -> per-stage model validation -> `Runtime::submit`. An invalid graph is
//! therefore refused before any admission on both paths; the only
//! observable difference is the synthetic `host:workflow` call id on
//! host-originated runs.

use super::{WorkflowOutcome, WorkflowTool};
use crate::BackgroundStatus;
use orca_harness_core::{Model, ToolError};
use orca_harness_dag::{Dag, RunId, RunState, Stage, StageId, StageStatus, DEFAULT_STAGE_CAP};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

/// One dependency graph to run, as a host names it. Stages are the
/// `harness-dag` [`Stage`] type: there is no second graph schema.
#[derive(Clone, Debug)]
pub struct WorkflowSubmission {
    stages: Vec<Stage>,
    max_stages: Option<usize>,
    timeout: Option<Duration>,
    resume_from: Option<RunId>,
}

impl WorkflowSubmission {
    pub fn new(stages: impl IntoIterator<Item = Stage>) -> Self {
        Self {
            stages: stages.into_iter().collect(),
            max_stages: None,
            timeout: None,
            resume_from: None,
        }
    }

    /// Bound on submitted and expanded stages (the tool's `maxStages`;
    /// [`DEFAULT_STAGE_CAP`] when unset). Zero is refused at submission.
    pub fn max_stages(mut self, cap: usize) -> Self {
        self.max_stages = Some(cap);
        self
    }

    /// Bound on the whole run including queue time (the tool's
    /// `timeoutSeconds`). Zero is refused at submission.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Reuse matching outputs from that prior run of this session (the
    /// tool's `resumeFrom`).
    pub fn resume_from(mut self, run: RunId) -> Self {
        self.resume_from = Some(run);
        self
    }

    pub fn stages(&self) -> &[Stage] {
        &self.stages
    }
}

/// An admitted run: the typed form of the tool's `run` acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowAcknowledgement {
    pub run_id: RunId,
    /// Stages in the submitted graph (map fan-out is not counted yet).
    pub stages: usize,
}

impl WorkflowAcknowledgement {
    /// The JSON the `workflow` tool returns for `action=run`.
    pub fn into_value(self) -> Value {
        json!({
            "runId": self.run_id,
            "status": "running",
            "stages": self.stages,
            "termination": "detached",
        })
    }
}

/// One admitted stage worker of a live run, running or queued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowStageJob {
    pub spawn_id: u64,
    /// The graph stage this worker executes.
    pub stage: Option<StageId>,
    pub status: BackgroundStatus,
}

/// Where one run stands: live (`outcome` is `None`) or finished.
///
/// For a live run admitted through another tool built over the same
/// manager (a host that rebuilt its tools mid-run), `state` is
/// `Running` and `stages` is empty: only the runtime that owns the DAG
/// knows its per-stage statuses. `active` and cancellation work from
/// any tool over the manager.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowStatus {
    pub run_id: RunId,
    pub state: RunState,
    pub stages: BTreeMap<StageId, StageStatus>,
    pub active: Vec<WorkflowStageJob>,
    /// The finished outcome the runtime recorded and delivered: the
    /// `harness-dag` outcome plus the runtime's timing and admission
    /// bookkeeping, the same document the parent receives as JSON.
    pub outcome: Option<WorkflowOutcome>,
}

/// One stage's stored output: the typed form of the tool's `output` result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageOutput {
    pub run_id: RunId,
    pub stage: StageId,
    pub answer: String,
}

impl StageOutput {
    /// The JSON the `workflow` tool returns for `action=output`.
    pub fn into_value(self) -> Value {
        json!({
            "runId": self.run_id,
            "stage": self.stage,
            "answer": self.answer,
        })
    }
}

impl<M: Model + Clone + 'static> WorkflowTool<M> {
    /// Validate and admit one graph, returning its acknowledgement at
    /// once. The run's terminal outcome reaches the subagent notifier as
    /// a run-level notification (`spawn.run` is `None`); stage
    /// completions reach it too but are never owed to the parent.
    ///
    /// Host-originated runs are subject to the same graph validation,
    /// per-stage model validation, admission limits, and reuse semantics
    /// as model-originated ones. Fails, with the tool's messages, before
    /// anything is admitted when the graph, a stage model, or an option
    /// is invalid.
    pub fn submit(
        &self,
        submission: WorkflowSubmission,
    ) -> Result<WorkflowAcknowledgement, ToolError> {
        self.admit(submission, "host:workflow".into())
    }

    /// Every live run over the session's manager, in run-id order.
    ///
    /// Unlike the tool's `action=list`, this never refuses a repeat with
    /// "workflow list unchanged": that refusal keeps the model from
    /// polling, whereas a host may poll this freely.
    pub fn runs(&self) -> Vec<WorkflowStatus> {
        self.runtime.statuses()
    }

    /// One run, live or finished; `None` when this session never
    /// admitted it (or the store was cleared with the conversation).
    pub fn status(&self, run_id: RunId) -> Option<WorkflowStatus> {
        self.runtime.status(run_id)
    }

    /// One stage's output, available once the stage completed (also
    /// while the rest of the run is still going). Fails with the tool's
    /// message when the run or stage is unknown or not yet done.
    pub fn stage_output(&self, run_id: RunId, stage: &str) -> Result<StageOutput, ToolError> {
        self.runtime.output(run_id, stage)
    }

    /// Stop one live run; its workers are cancelled and the run settles
    /// as `Cancelled`. Fails with "workflow is not running" otherwise.
    pub fn cancel(&self, run_id: RunId) -> Result<(), ToolError> {
        self.runtime.cancel(run_id)
    }

    /// The single submission path shared by the model tool and the
    /// typed API: option checks, graph validation, model validation,
    /// then admission. Nothing is admitted until every check passed.
    pub(super) fn admit(
        &self,
        submission: WorkflowSubmission,
        call_id: String,
    ) -> Result<WorkflowAcknowledgement, ToolError> {
        let cap = match submission.max_stages {
            Some(0) => return Err(ToolError::msg("maxStages must be a positive integer")),
            Some(cap) => cap,
            None => DEFAULT_STAGE_CAP,
        };
        if submission.timeout.is_some_and(|timeout| timeout.is_zero()) {
            return Err(ToolError::msg("timeoutSeconds must be positive"));
        }
        let dag =
            Dag::from_stages(submission.stages, cap).map_err(|e| ToolError::msg(e.to_string()))?;
        for stage in dag.stages() {
            self.runtime
                .subagent
                .validate_model(stage.model.as_deref())?;
        }
        self.runtime
            .submit(dag, call_id, submission.resume_from, submission.timeout)
    }
}
