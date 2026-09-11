//! The agent-level subagent recipe and the session-level host handle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::{CancellationToken, Limits, Model};
use orca_harness_extensions::HarnessEvent;
use orca_harness_tools::{
    BackgroundAcknowledgement, BackgroundJob, CompletionInbox, SpawnExtensions, SubagentDepth,
    SubagentModel, SubagentOutcome, SubagentRequest, SubagentSpawn, SubagentTool,
};
use tokio::sync::broadcast;

use super::BackgroundNotification;
use crate::SdkError;

/// Observes one child's harness events, tagged with the spawn they
/// belong to; see [`SubagentConfig::on_child_event`].
pub type ChildEventCallback = Arc<dyn Fn(&SubagentSpawn, HarnessEvent) + Send + Sync>;

/// Provider recorded for inherited workers when the agent did not name one.
pub(super) const DEFAULT_IDENTITY_PROVIDER: &str = "sdk";

/// How an agent's sessions spawn subagents. Attach with
/// [`AgentBuilder::subagents`](crate::AgentBuilder::subagents).
///
/// `settings` is a live handle shared by every session of the agent (the
/// nesting cap, worker limits, routing, and the background concurrency
/// limit can be adjusted mid-session through it); managers, queues, and
/// completion inboxes are still built per session.
///
/// What a child runs with, in registration order: the
/// [`on_child_event`](Self::on_child_event) relay, the agent's own
/// extensions when [`inherit_extensions`](Self::inherit_extensions) is on,
/// the [`child_extensions`](Self::child_extensions) factory's output, and
/// output truncation sized by the live `output_chars` setting. A child
/// never gets the parent's session recorder, usage meter, event stream,
/// truncation store, compaction, or skill tracking: those are built per
/// parent run and are not reachable from here.
#[derive(Clone)]
pub struct SubagentConfig {
    pub(crate) settings: SubagentDepth,
    pub(crate) models: Vec<SubagentModel<Arc<dyn Model>>>,
    pub(crate) system_prompt: Option<String>,
    pub(crate) limits: Option<Limits>,
    pub(crate) identity: Option<(String, String)>,
    pub(crate) background_limit: Option<u32>,
    pub(crate) max_depth: Option<u32>,
    pub(crate) inherit_extensions: bool,
    pub(crate) child_extensions: Option<SpawnExtensions>,
    pub(crate) on_child_event: Option<ChildEventCallback>,
    pub(crate) workflows: bool,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            settings: SubagentDepth::default(),
            models: Vec::new(),
            system_prompt: None,
            limits: None,
            identity: None,
            background_limit: None,
            max_depth: None,
            inherit_extensions: true,
            child_extensions: None,
            on_child_event: None,
            workflows: true,
        }
    }
}

impl SubagentConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether sessions also get the `workflow` model tool and the typed
    /// [`Workflows`](crate::Workflows) handle over the same subagent
    /// manager. On by default: configuring subagents registers the
    /// `workflow` tool alongside `subagent` unless this is `false`, which
    /// leaves plain subagents only and makes
    /// [`Session::workflows`](crate::Session::workflows) return `None`.
    pub fn workflows(mut self, enabled: bool) -> Self {
        self.workflows = enabled;
        self
    }

    /// Whether every child gets the agent's own extensions (those added
    /// with [`AgentBuilder::extension`], [`extension_arc`], and
    /// [`policy`]). On by default, so a restriction that holds for the
    /// parent holds for its workers too. The child receives the same
    /// instances the parent runs with, not copies, so anything registered
    /// on the agent must tolerate concurrent use from several agents
    /// ([`ToolPolicy`](crate::ToolPolicy) does).
    ///
    /// [`AgentBuilder::extension`]: crate::AgentBuilder::extension
    /// [`extension_arc`]: crate::AgentBuilder::extension_arc
    /// [`policy`]: crate::AgentBuilder::policy
    pub fn inherit_extensions(mut self, inherit: bool) -> Self {
        self.inherit_extensions = inherit;
        self
    }

    /// Build host extensions for each spawned child (nested ones too),
    /// registered after the inherited agent extensions and before output
    /// truncation. The factory runs once per spawn, on the spawning
    /// task, and may return fresh instances every time.
    pub fn child_extensions(mut self, factory: SpawnExtensions) -> Self {
        self.child_extensions = Some(factory);
        self
    }

    /// Observe each child's harness events (start, model turns, tool
    /// calls, end) tagged with its [`SubagentSpawn`]. Installed as the
    /// first extension of every child, so it sees the whole run.
    pub fn on_child_event(
        mut self,
        callback: impl Fn(&SubagentSpawn, HarnessEvent) + Send + Sync + 'static,
    ) -> Self {
        self.on_child_event = Some(Arc::new(callback));
        self
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

    /// Fixed worker limits, applied to each spawn. Without this, workers
    /// follow the live `settings` step budget. Explicit limits do not
    /// rewrite the shared `settings` handle: a session opening with them
    /// leaves the step budget other sessions read unchanged.
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
    events: broadcast::Sender<BackgroundNotification>,
    /// The session's shutdown flag; see [`Session::shutdown`].
    ///
    /// [`Session::shutdown`]: crate::Session::shutdown
    closed: Arc<AtomicBool>,
}

impl Subagents {
    pub(crate) fn new(
        tool: Arc<SubagentTool<Arc<dyn Model>>>,
        inbox: CompletionInbox,
        settings: SubagentDepth,
        events: broadcast::Sender<BackgroundNotification>,
        closed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            tool,
            inbox,
            settings,
            events,
            closed,
        }
    }

    fn ensure_open(&self) -> Result<(), SdkError> {
        if self.closed.load(Ordering::Acquire) {
            Err(SdkError::SessionClosed)
        } else {
            Ok(())
        }
    }

    /// Admit a detached worker and return at once. Its result lands in the
    /// session's completion inbox (see [`pending_completions`]). Refused
    /// with [`SdkError::SessionClosed`] once the session was shut down.
    ///
    /// [`pending_completions`]: Self::pending_completions
    pub fn spawn(&self, request: SubagentRequest) -> Result<BackgroundAcknowledgement, SdkError> {
        self.ensure_open()?;
        self.tool
            .spawn_background(request)
            .map_err(|error| SdkError::Subagent(error.to_string()))
    }

    /// Run one worker in the foreground and return its typed outcome. A
    /// missing `cancellation` means the run can only end on its own; the
    /// `deadline` is measured from now and combined with the configured
    /// limits and the live worker timeout. Refused with
    /// [`SdkError::SessionClosed`] once the session was shut down.
    pub async fn run(
        &self,
        request: SubagentRequest,
        cancellation: Option<CancellationToken>,
        deadline: Option<Duration>,
    ) -> Result<SubagentOutcome, SdkError> {
        self.ensure_open()?;
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

    /// A fresh observer of this session's detached work, for code that
    /// holds only this handle. [`Session::notifications`] is the primary
    /// entry point and documents the channel's contract; this is the same
    /// channel.
    ///
    /// [`Session::notifications`]: crate::Session::notifications
    pub fn notifications(&self) -> broadcast::Receiver<BackgroundNotification> {
        self.events.subscribe()
    }
}
