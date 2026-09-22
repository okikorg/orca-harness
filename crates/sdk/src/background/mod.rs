//! Session-owned background services: the subagent manager, the completion
//! inbox detached results land in, and the `subagent` tool that feeds
//! both, reporting on the session's notification channel (which the
//! session owns, so background processes report there as well).
//!
//! Built once per session when the agent configured subagents (see
//! [`AgentBuilder::subagents`](crate::AgentBuilder::subagents)), so two
//! sessions never share live jobs, undelivered results, or observers. A
//! fork or a resume from disk builds fresh services: only the transcript
//! carries over. The host reaches them through
//! [`Session::subagents`](crate::Session::subagents) and
//! [`Session::notifications`](crate::Session::notifications).
//!
//! Lifecycle: the end of a parent run is not a shutdown, and cancelling a
//! run cancels that run only; detached workers continue. `clear`,
//! `reset_in_place`, and [`Session::shutdown`](crate::Session::shutdown)
//! cancel every worker and start a new generation, which invalidates
//! results still on their way. Dropping the session does the same on a
//! best-effort basis (see [`BackgroundServices`]'s `Drop`).

mod notifications;
mod processes;
mod subagents;
mod workflows;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use orca_harness_core::{Extension, Model, Tool};
use orca_harness_extensions::{EventStream, Truncation};
use orca_harness_tools::{
    CompletionInbox, FileGuard, SpawnExtensions, SubagentDepth, SubagentManager, SubagentTool,
    WorkflowStore, WorkflowTool,
};
use tokio::sync::broadcast;

use crate::agent::AgentDefinition;
use crate::tools::{preset_tools, ToolSource};

pub use notifications::{BackgroundNotification, NOTIFICATION_CAPACITY};
pub use processes::{ProcessConfig, Processes};
pub use subagents::{ChildEventCallback, SubagentConfig, Subagents};
pub use workflows::Workflows;

pub(crate) use notifications::rearm;

/// The handles one run needs to deliver completions and refresh the
/// worker inventory at its model boundaries: small clones, not the
/// services themselves.
pub(crate) struct RunBackground {
    pub(crate) inbox: CompletionInbox,
    pub(crate) events: broadcast::Sender<BackgroundNotification>,
}

pub(crate) struct BackgroundServices {
    inbox: CompletionInbox,
    settings: SubagentDepth,
    subagents: Arc<SubagentTool<Arc<dyn Model>>>,
    /// The `workflow` tool over the same manager, when the recipe enables
    /// workflows. Its stage-output store is shared with the inbox so a
    /// conversation reset discards the outputs with the runs.
    workflows: Option<Arc<WorkflowTool<Arc<dyn Model>>>>,
    events: broadcast::Sender<BackgroundNotification>,
}

impl BackgroundServices {
    /// Materialize the agent's subagent recipe for one session.
    ///
    /// Children run with the agent's tool preset (and its process
    /// configuration: executor and limits, but no notifier, since a
    /// child's processes have no host to report to) over a fresh
    /// read-before-write guard per spawn plus the caller-owned custom
    /// tools; they get no REPLs, todo list, skills, MCP, or memory tools.
    /// Nesting is governed by the shared [`SubagentDepth`] handle, as for
    /// the model tool.
    ///
    /// Child extensions come from [`spawn_extensions`] (see
    /// [`SubagentConfig::inherit_extensions`] for the ordering). The
    /// parent's per-run extensions (recorder, usage meter, event stream,
    /// truncation store, compaction, `SkillOnce`, completion delivery) are
    /// built inside each run, not here, so no child can ever share them.
    pub(crate) fn new(
        definition: &AgentDefinition,
        config: &SubagentConfig,
        events: broadcast::Sender<BackgroundNotification>,
    ) -> Self {
        let settings = config.settings.clone();
        let manager = SubagentManager::from_settings(settings.clone());
        let store = config.workflows.then(WorkflowStore::new);
        let mut inbox = CompletionInbox::new(manager.clone());
        if let Some(store) = &store {
            inbox = inbox.with_workflow_store(store.clone());
        }
        let workspace = definition.harness.workspace().clone();
        let preset = definition.preset;
        let processes = definition.processes.clone();
        let custom: Vec<Arc<dyn Tool>> = definition
            .tool_sources
            .iter()
            .filter_map(|source| match source {
                ToolSource::Custom(tool) => Some(tool.clone()),
                _ => None,
            })
            .collect();
        let factory = Arc::new(move || {
            let mut tools = preset_tools(
                preset,
                &workspace,
                &FileGuard::default(),
                processes.as_ref(),
            );
            tools.extend(custom.iter().cloned());
            tools
        });
        let notifier = (inbox.clone(), events.clone());
        let (provider, model) = config.identity.clone().unwrap_or_else(|| {
            (
                subagents::DEFAULT_IDENTITY_PROVIDER.to_string(),
                definition.model_name.clone(),
            )
        });
        let mut tool = SubagentTool::with_tools(definition.model.clone(), factory)
            .inherited_identity(provider, model)
            .models(config.models.iter().cloned())
            .background(manager, move |notification| {
                notifications::notify(&notifier.0, &notifier.1, notification);
            })
            .spawn_extensions(spawn_extensions(definition, config))
            // Share the handle before applying explicit limits: `max_depth`
            // publishes configured limits into the shared handle, and
            // explicit limits are per spawn, not a rewrite of what every
            // session of the agent sees.
            .max_depth(settings.clone());
        if let Some(prompt) = &config.system_prompt {
            tool = tool.system_prompt(prompt.clone());
        }
        if let Some(limits) = &config.limits {
            tool = tool.limits(limits.clone());
        }
        // Attempts and backoff come from the live settings handle (see
        // `SubagentConfig::apply_to_settings`, or the host's own edits);
        // the tool carries the classifiers, so whenever children retry
        // they retry exactly what the parent's layer would: data failures
        // in, native file mutations out.
        tool = tool
            .retry_ok_when(orca_harness_tools::retry::data_failure)
            .retry_error_when(orca_harness_tools::retry::retryable_error);
        let subagents = Arc::new(tool);
        let workflows = store.map(|store| {
            // Only fails without depth-zero background execution, which
            // `background` above configured on this very tool.
            let tool = WorkflowTool::new(subagents.clone(), store)
                .expect("session subagents run with background execution");
            Arc::new(tool)
        });
        Self {
            inbox,
            settings,
            subagents,
            workflows,
            events,
        }
    }

    /// The tools registered in the session's tool list: `workflow` first
    /// when enabled, then `subagent`, in the order the CLI registers them.
    pub(crate) fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        if let Some(workflows) = &self.workflows {
            tools.push(workflows.clone());
        }
        tools.push(self.subagents.clone());
        tools
    }

    pub(crate) fn handle(&self, closed: Arc<AtomicBool>) -> Subagents {
        Subagents::new(
            self.subagents.clone(),
            self.inbox.clone(),
            self.settings.clone(),
            self.events.clone(),
            closed,
        )
    }

    pub(crate) fn workflows(&self, closed: Arc<AtomicBool>) -> Option<Workflows> {
        self.workflows
            .as_ref()
            .map(|tool| Workflows::new(tool.clone(), closed))
    }

    pub(crate) fn run_handles(&self) -> RunBackground {
        RunBackground {
            inbox: self.inbox.clone(),
            events: self.events.clone(),
        }
    }

    pub(crate) fn pending_completions(&self) -> usize {
        self.inbox.pending()
    }

    pub(crate) fn manager(&self) -> &SubagentManager {
        self.inbox.manager()
    }

    /// Cancel every detached worker, drop undelivered results, and start
    /// a new generation so results of workers still winding down are
    /// refused rather than delivered to the replaced conversation.
    pub(crate) fn clear(&self) {
        self.subagents.stop_all_sidekicks();
        self.inbox.reset();
    }

    /// Refuse every later admission, then cancel and clear as
    /// [`clear`](Self::clear) does.
    pub(crate) fn close(&self) {
        self.subagents.stop_all_sidekicks();
        self.inbox.manager().close();
        self.inbox.reset();
    }
}

/// Best-effort cleanup when the session goes away without an explicit
/// shutdown. The notifier closure held by each in-flight job keeps the
/// manager alive, so its own lifetime guard would not fire until the
/// last worker exits; cancelling here makes that exit prompt for
/// workers that honour their token.
impl Drop for BackgroundServices {
    fn drop(&mut self) {
        self.subagents.stop_all_sidekicks();
        self.inbox.reset();
    }
}

/// The per-spawn extension factory installed on the session's `subagent`
/// tool, in the order a child registers them: the host's event relay,
/// the agent's inherited extensions, the host's per-spawn extensions,
/// then output truncation sized from the live settings.
fn spawn_extensions(definition: &AgentDefinition, config: &SubagentConfig) -> SpawnExtensions {
    let inherited: Vec<Arc<dyn Extension>> = if config.inherit_extensions {
        definition.extensions.clone()
    } else {
        Vec::new()
    };
    let host = config.child_extensions.clone();
    let relay = config.on_child_event.clone();
    let settings = config.settings.clone();
    Arc::new(move |spawn| {
        let mut extensions: Vec<Arc<dyn Extension>> = Vec::new();
        if let Some(relay) = &relay {
            let relay = relay.clone();
            let spawn = spawn.clone();
            extensions.push(Arc::new(EventStream::from_fn(move |event| {
                relay(&spawn, event)
            })));
        }
        extensions.extend(inherited.iter().cloned());
        if let Some(host) = &host {
            extensions.extend(host(spawn));
        }
        // Stateless and per child: never the parent's truncation store.
        let output_chars = settings.output_chars();
        if output_chars != 0 {
            extensions.push(Arc::new(Truncation::new(output_chars as usize)));
        }
        extensions
    })
}
