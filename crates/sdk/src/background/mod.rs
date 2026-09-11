//! Session-owned background services: the subagent manager, the completion
//! inbox detached results land in, and the `subagent` tool that feeds both.
//!
//! Built once per session when the agent configured subagents (see
//! [`AgentBuilder::subagents`](crate::AgentBuilder::subagents)), so two
//! sessions never share live jobs or undelivered results. The host reaches
//! them through [`Session::subagents`](crate::Session::subagents).

mod subagents;

use std::sync::Arc;

use orca_harness_core::{Model, Tool};
use orca_harness_tools::{
    CompletionInbox, FileGuard, SubagentDepth, SubagentManager, SubagentTool,
};

use crate::agent::AgentDefinition;
use crate::tools::{preset_tools, ToolSource};

pub use subagents::{SubagentConfig, Subagents};

pub(crate) struct BackgroundServices {
    inbox: CompletionInbox,
    settings: SubagentDepth,
    subagents: Arc<SubagentTool<Arc<dyn Model>>>,
}

impl BackgroundServices {
    /// Materialize the agent's subagent recipe for one session.
    ///
    /// Children run with the agent's tool preset over a fresh
    /// read-before-write guard per spawn plus the caller-owned custom
    /// tools; they get no REPLs, todo list, skills, MCP, or memory tools.
    /// Nesting is governed by the shared [`SubagentDepth`] handle, as for
    /// the model tool.
    pub(crate) fn new(definition: &AgentDefinition, config: &SubagentConfig) -> Self {
        let settings = config.settings.clone();
        let manager = SubagentManager::from_settings(settings.clone());
        let inbox = CompletionInbox::new(manager.clone());
        let workspace = definition.harness.workspace().clone();
        let preset = definition.preset;
        let custom: Vec<Arc<dyn Tool>> = definition
            .tool_sources
            .iter()
            .filter_map(|source| match source {
                ToolSource::Custom(tool) => Some(tool.clone()),
                _ => None,
            })
            .collect();
        let factory = Arc::new(move || {
            let mut tools = preset_tools(preset, &workspace, &FileGuard::default());
            tools.extend(custom.iter().cloned());
            tools
        });
        let notify = inbox.clone();
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
                notify.push(notification);
            });
        if let Some(prompt) = &config.system_prompt {
            tool = tool.system_prompt(prompt.clone());
        }
        if let Some(limits) = &config.limits {
            tool = tool.limits(limits.clone());
        }
        // B3: child policy inheritance (spawn_extensions from the agent's
        // extensions) is deliberately not attached here.
        let tool = tool.max_depth(settings.clone());
        Self {
            inbox,
            settings,
            subagents: Arc::new(tool),
        }
    }

    /// The `subagent` tool registered in the session's tool list.
    pub(crate) fn tool(&self) -> Arc<dyn Tool> {
        self.subagents.clone()
    }

    pub(crate) fn handle(&self) -> Subagents {
        Subagents::new(
            self.subagents.clone(),
            self.inbox.clone(),
            self.settings.clone(),
        )
    }

    /// Cancel every detached worker and drop undelivered results, as part
    /// of replacing the session's conversation.
    pub(crate) fn clear(&self) {
        self.inbox.reset();
    }
}
