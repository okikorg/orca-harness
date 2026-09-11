//! Tool presets and per-session tool construction. An agent records a
//! recipe (preset plus builder-ordered sources); each session materializes
//! it so mutable built-ins (file guard, todos, processes, REPLs) are owned
//! by that session alone.

use std::sync::Arc;

use orca_harness_core::Tool;
use orca_harness_tools::{
    core_tools_with_process, fs_admin_tools, BunReplTool, EditFileTool, FileGuard, GlobTool,
    GrepTool, ListDirTool, ProcessController, ProcessTool, PyKernelTool, ReadFileTool, TodoList,
    TodoWriteTool, Workspace, WriteFileTool,
};

use crate::agent::AgentDefinition;
use crate::background::BackgroundServices;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolPreset {
    #[default]
    None,
    ReadOnly,
    Coding,
    ShellLess,
}

/// One builder call that contributes tools, recorded so a session
/// registers them in builder call order. `Custom` tools are caller-owned
/// and shared by every session; the other variants are built fresh per
/// session.
pub(crate) enum ToolSource {
    Custom(Arc<dyn Tool>),
    Python,
    Bun,
    Todos,
}

/// The preset's tools alone, for callers (subagent children) that keep
/// no host handle over the process tool.
pub(crate) fn preset_tools(
    preset: ToolPreset,
    workspace: &Workspace,
    guard: &FileGuard,
) -> Vec<Arc<dyn Tool>> {
    preset_tools_with_process(preset, workspace, guard).0
}

/// The preset's tools plus, for a preset that ships the `process` tool,
/// the typed controller over that same tool. The tool is configured once
/// and registered as built, so the controller and the model tool share
/// one manager.
pub(crate) fn preset_tools_with_process(
    preset: ToolPreset,
    workspace: &Workspace,
    guard: &FileGuard,
) -> (Vec<Arc<dyn Tool>>, Option<ProcessController>) {
    let tools: Vec<Arc<dyn Tool>> = match preset {
        ToolPreset::None => Vec::new(),
        ToolPreset::ReadOnly => vec![
            Arc::new(ReadFileTool::new(workspace.clone()).guard(guard.clone())),
            Arc::new(ListDirTool::new(workspace.clone())),
            Arc::new(GrepTool::new(workspace.clone())),
            Arc::new(GlobTool::new(workspace.clone())),
        ],
        ToolPreset::Coding => {
            // C2: notifications — `.on_notification(..)` and the host's
            // process configuration attach here, before the controller
            // is taken.
            let process = ProcessTool::local().working_dir(workspace.root().display().to_string());
            let controller = process.controller();
            return (
                core_tools_with_process(workspace, guard, process),
                Some(controller),
            );
        }
        ToolPreset::ShellLess => {
            let mut tools: Vec<Arc<dyn Tool>> = vec![
                Arc::new(ReadFileTool::new(workspace.clone()).guard(guard.clone())),
                Arc::new(WriteFileTool::new(workspace.clone()).guard(guard.clone())),
                Arc::new(EditFileTool::new(workspace.clone()).guard(guard.clone())),
                Arc::new(ListDirTool::new(workspace.clone())),
                Arc::new(GrepTool::new(workspace.clone())),
                Arc::new(GlobTool::new(workspace.clone())),
            ];
            tools.extend(fs_admin_tools(workspace));
            tools
        }
    };
    (tools, None)
}

/// The tools one session runs with, plus the mutable built-in state
/// behind them. Built at open/resume/fork so two sessions never share a
/// process manager, REPL, guard, or todo list unless the agent was
/// configured with a caller-owned instance.
pub(crate) struct SessionTools {
    pub(crate) file_guard: FileGuard,
    pub(crate) todo_list: Option<TodoList>,
    /// Present when the agent configured subagents; owns this session's
    /// manager, completion inbox, and `subagent` tool.
    pub(crate) background: Option<BackgroundServices>,
    /// Present when the preset ships the `process` tool; the typed host
    /// handle over this session's own process manager.
    pub(crate) process: Option<ProcessController>,
    pub(crate) tools: Vec<Arc<dyn Tool>>,
}

impl SessionTools {
    /// Materializes the recipe: preset tools wired to the session's guard,
    /// then the builder-ordered sources, then the session's `subagent`
    /// tool (registered last among the host-built tools, as the CLI does),
    /// then the agent-level tools (skills, MCP, memory) that are shared by
    /// design.
    pub(crate) fn new(definition: &AgentDefinition) -> Self {
        let workspace = definition.harness.workspace();
        let working_dir = || workspace.root().display().to_string();
        let file_guard = definition.shared_file_guard.clone().unwrap_or_default();
        let mut todo_list: Option<TodoList> = None;
        let (mut tools, process) =
            preset_tools_with_process(definition.preset, workspace, &file_guard);
        for source in &definition.tool_sources {
            tools.push(match source {
                ToolSource::Custom(tool) => tool.clone(),
                ToolSource::Python => Arc::new(PyKernelTool::new().working_dir(working_dir())),
                ToolSource::Bun => Arc::new(BunReplTool::new().working_dir(working_dir())),
                ToolSource::Todos => {
                    let list = todo_list.get_or_insert_with(|| {
                        definition.shared_todo_list.clone().unwrap_or_default()
                    });
                    Arc::new(TodoWriteTool::new(list.clone()))
                }
            });
        }
        let background = definition
            .subagents
            .as_ref()
            .map(|config| BackgroundServices::new(definition, config));
        if let Some(services) = &background {
            tools.push(services.tool());
        }
        tools.extend(definition.shared_tools.iter().cloned());
        Self {
            file_guard,
            todo_list,
            background,
            process,
            tools,
        }
    }

    /// Reset every session-owned built-in: the guard, todos, and detached
    /// subagents with their undelivered results.
    pub(crate) fn clear(&self) {
        self.file_guard.clear();
        if let Some(todos) = &self.todo_list {
            todos.clear();
        }
        if let Some(background) = &self.background {
            background.clear();
        }
    }
}
