//! The agent-level subagent recipe and the session-level host handle.

use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::{CancellationToken, Limits, Model};
use orca_harness_tools::{
    BackgroundAcknowledgement, BackgroundJob, CompletionInbox, SubagentDepth, SubagentModel,
    SubagentOutcome, SubagentRequest, SubagentTool,
};

use crate::SdkError;

/// Provider recorded for inherited workers when the agent did not name one.
pub(super) const DEFAULT_IDENTITY_PROVIDER: &str = "sdk";

/// How an agent's sessions spawn subagents. Attach with
/// [`AgentBuilder::subagents`](crate::AgentBuilder::subagents).
///
/// `settings` is a live handle shared by every session of the agent (the
/// nesting cap, worker limits, routing, and the background concurrency
/// limit can be adjusted mid-session through it); managers, queues, and
/// completion inboxes are still built per session.
#[derive(Clone, Default)]
pub struct SubagentConfig {
    pub(crate) settings: SubagentDepth,
    pub(crate) models: Vec<SubagentModel<Arc<dyn Model>>>,
    pub(crate) system_prompt: Option<String>,
    pub(crate) limits: Option<Limits>,
    pub(crate) identity: Option<(String, String)>,
    pub(crate) background_limit: Option<u32>,
    pub(crate) max_depth: Option<u32>,
}

impl SubagentConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Share an existing live settings handle instead of a fresh default.
    pub fn settings(mut self, settings: SubagentDepth) -> Self {
        self.settings = settings;
        self
    }

    /// Offer one host-approved model workers may be routed to.
    pub fn model(mut self, model: SubagentModel<Arc<dyn Model>>) -> Self {
        self.models.push(model);
        self
    }

    pub fn models(
        mut self,
        models: impl IntoIterator<Item = SubagentModel<Arc<dyn Model>>>,
    ) -> Self {
        self.models.extend(models);
        self
    }

    /// Default system prompt for workers; a request's own prompt overrides it.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Fixed worker limits. Without this, workers follow the live
    /// `settings` step budget.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Provider and model recorded for workers that inherit the agent's
    /// model. Defaults to `"sdk"` and the agent's name.
    pub fn inherited_identity(
        mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.identity = Some((provider.into(), model.into()));
        self
    }

    /// Detached workers allowed to run at once; the rest queue in spawn
    /// order. Zero means unrestricted. Applied to `settings` when the
    /// agent is built.
    pub fn background_limit(mut self, limit: u32) -> Self {
        self.background_limit = Some(limit);
        self
    }

    /// Nesting cap: workers get their own `subagent` tool while their
    /// depth plus one is below it. Applied to `settings` when the agent is
    /// built.
    pub fn max_depth(mut self, depth: u32) -> Self {
        self.max_depth = Some(depth);
        self
    }

    /// Push the one-shot builder choices into the live handle.
    pub(crate) fn apply_to_settings(&self) {
        if let Some(limit) = self.background_limit {
            self.settings.set_background_limit(limit);
        }
        if let Some(depth) = self.max_depth {
            self.settings.set(depth);
        }
    }
}

/// Host handle over one session's subagents. Every operation runs the same
/// implementation as the session's `subagent` model tool, so host spawns
/// are validated against the live route, admitted through the session's
/// concurrency limit and completion capacity, and carry the same
/// identity, ids, and cancellation semantics as model-originated ones.
#[derive(Clone)]
pub struct Subagents {
    tool: Arc<SubagentTool<Arc<dyn Model>>>,
    inbox: CompletionInbox,
    settings: SubagentDepth,
}

impl Subagents {
    pub(crate) fn new(
        tool: Arc<SubagentTool<Arc<dyn Model>>>,
        inbox: CompletionInbox,
        settings: SubagentDepth,
    ) -> Self {
        Self {
            tool,
            inbox,
            settings,
        }
    }

    /// Admit a detached worker and return at once. Its result lands in the
    /// session's completion inbox (see [`pending_completions`]).
    ///
    /// [`pending_completions`]: Self::pending_completions
    pub fn spawn(&self, request: SubagentRequest) -> Result<BackgroundAcknowledgement, SdkError> {
        self.tool
            .spawn_background(request)
            .map_err(|error| SdkError::Subagent(error.to_string()))
    }

    /// Run one worker in the foreground and return its typed outcome. A
    /// missing `cancellation` means the run can only end on its own; the
    /// `deadline` is measured from now and combined with the configured
    /// limits and the live worker timeout.
    pub async fn run(
        &self,
        request: SubagentRequest,
        cancellation: Option<CancellationToken>,
        deadline: Option<Duration>,
    ) -> Result<SubagentOutcome, SdkError> {
        let deadline = deadline.map(|duration| tokio::time::Instant::now() + duration);
        self.tool
            .run_foreground(request, cancellation.unwrap_or_default(), deadline)
            .await
            .map_err(|error| SdkError::Subagent(error.to_string()))
    }

    /// Admitted detached workers, running and queued, in spawn order.
    pub fn active(&self) -> Vec<BackgroundJob> {
        self.tool.active_jobs()
    }

    /// Cancel one detached worker; `false` when it is unknown or finished.
    pub fn cancel(&self, spawn_id: u64) -> bool {
        self.tool.cancel_job(spawn_id)
    }

    /// Cancel every detached worker and report how many were admitted.
    pub fn cancel_all(&self) -> usize {
        self.tool.cancel_all_jobs()
    }

    /// The live settings handle (limits, routing, nesting cap,
    /// background concurrency), shared with the model tool.
    pub fn settings(&self) -> SubagentDepth {
        self.settings.clone()
    }

    /// Completed detached results not yet delivered to the conversation.
    pub fn pending_completions(&self) -> usize {
        self.inbox.pending()
    }
}
