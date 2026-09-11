//! The session-level host handle over workflow runs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use orca_harness_core::Model;
use orca_harness_tools::dag::RunId;
use orca_harness_tools::{
    StageOutput, WorkflowAcknowledgement, WorkflowStatus, WorkflowSubmission, WorkflowTool,
};

use crate::SdkError;

/// Host handle over one session's workflow runs, when the agent
/// configured subagents with workflows enabled (the default; see
/// [`SubagentConfig::workflows`]). Every operation runs the same
/// implementation as the session's `workflow` model tool over the same
/// session-owned subagent manager and stage-output store, so host- and
/// model-submitted runs share ids, admission limits, model routing,
/// `resume_from` reuse, and cancellation.
///
/// A run's stage completions are observable through
/// [`Session::notifications`] as [`SubagentFinished`] with
/// `spawn.run.is_some()`, but only the run-level outcome (`spawn.run`
/// is `None`, `spawn.id` is the run id) is owed to the parent
/// transcript and counted by [`Session::pending_completions`]. Every
/// call fails with [`SdkError::SessionClosed`] once the session was
/// shut down.
///
/// [`SubagentConfig::workflows`]: crate::SubagentConfig::workflows
/// [`Session::notifications`]: crate::Session::notifications
/// [`Session::pending_completions`]: crate::Session::pending_completions
/// [`SubagentFinished`]: crate::BackgroundNotification::SubagentFinished
#[derive(Clone)]
pub struct Workflows {
    tool: Arc<WorkflowTool<Arc<dyn Model>>>,
    /// The session's shutdown flag; see [`Session::shutdown`].
    ///
    /// [`Session::shutdown`]: crate::Session::shutdown
    closed: Arc<AtomicBool>,
}

impl Workflows {
    pub(crate) fn new(tool: Arc<WorkflowTool<Arc<dyn Model>>>, closed: Arc<AtomicBool>) -> Self {
        Self { tool, closed }
    }

    fn ensure_open(&self) -> Result<(), SdkError> {
        if self.closed.load(Ordering::Acquire) {
            Err(SdkError::SessionClosed)
        } else {
            Ok(())
        }
    }

    /// Validate and admit one graph, returning its acknowledgement at
    /// once. An invalid graph, stage model, or option is refused with
    /// [`SdkError::Workflow`] before anything is admitted.
    pub fn submit(
        &self,
        submission: WorkflowSubmission,
    ) -> Result<WorkflowAcknowledgement, SdkError> {
        self.ensure_open()?;
        self.tool.submit(submission).map_err(workflow_error)
    }

    /// Every live run, in run-id order. A host may poll this freely.
    pub fn runs(&self) -> Vec<WorkflowStatus> {
        self.tool.runs()
    }

    /// One run, live or finished; `None` when this session never
    /// admitted it or the conversation was cleared since.
    pub fn status(&self, run_id: RunId) -> Option<WorkflowStatus> {
        self.tool.status(run_id)
    }

    /// One stage's output once that stage completed, even while the rest
    /// of the run is still going.
    pub fn stage_output(&self, run_id: RunId, stage: &str) -> Result<StageOutput, SdkError> {
        self.ensure_open()?;
        self.tool
            .stage_output(run_id, stage)
            .map_err(workflow_error)
    }

    /// Stop one live run; it settles as cancelled and its outcome still
    /// reaches the parent.
    pub fn cancel(&self, run_id: RunId) -> Result<(), SdkError> {
        self.ensure_open()?;
        self.tool.cancel(run_id).map_err(workflow_error)
    }
}

fn workflow_error(error: orca_harness_core::ToolError) -> SdkError {
    SdkError::Workflow(error.to_string())
}
