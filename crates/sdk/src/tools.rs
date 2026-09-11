//! Tool presets and per-session tool construction. An agent records a
//! recipe (preset plus builder-ordered sources); each session materializes
//! it so mutable built-ins (file guard, todos, processes, REPLs) are owned
//! by that session alone.

use std::sync::Arc;

use orca_harness_core::Tool;
use orca_harness_tools::{
    core_tools_with_guard, fs_admin_tools, BunReplTool, EditFileTool, FileGuard, GlobTool,
    GrepTool, ListDirTool, PyKernelTool, ReadFileTool, TodoList, TodoWriteTool, Workspace,
    WriteFileTool,
};

use crate::agent::AgentDefinition;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolPreset {
    #[default]
    None,
    ReadOnly,
    Coding,
    ShellLess,
}

/// One builder call that contributes tools, recorded in call order so a
/// session registers them exactly as the agent-level list used to.
/// `Custom` tools are caller-owned and shared by every session; the other
/// variants are built fresh per session.
pub(crate) enum ToolSource {
    Custom(Arc<dyn Tool>),
    Python,
    Bun,
    Todos,
}

pub(crate) fn preset_tools(
    preset: ToolPreset,
    workspace: &Workspace,
    guard: &FileGuard,
) -> Vec<Arc<dyn Tool>> {
    match preset {
        ToolPreset::None => Vec::new(),
        ToolPreset::ReadOnly => vec![
            Arc::new(ReadFileTool::new(workspace.clone()).guard(guard.clone())),
            Arc::new(ListDirTool::new(workspace.clone())),
            Arc::new(GrepTool::new(workspace.clone())),
            Arc::new(GlobTool::new(workspace.clone())),
        ],
        ToolPreset::Coding => core_tools_with_guard(workspace, guard),
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
    }
}

/// Builds the full tool list for one session: preset tools wired to the
/// session's `guard`, then the builder-ordered sources, then the
/// agent-level tools (skills, MCP, memory) that are shared by design.
/// `todos` must be `Some` when the recipe contains [`ToolSource::Todos`].
pub(crate) fn session_tools(
    definition: &AgentDefinition,
    guard: &FileGuard,
    todos: Option<&TodoList>,
) -> Vec<Arc<dyn Tool>> {
    let workspace = definition.harness.workspace();
    let working_dir = || workspace.root().display().to_string();
    let mut tools = preset_tools(definition.preset, workspace, guard);
    for source in &definition.tool_sources {
        tools.push(match source {
            ToolSource::Custom(tool) => tool.clone(),
            ToolSource::Python => Arc::new(PyKernelTool::new().working_dir(working_dir())),
            ToolSource::Bun => Arc::new(BunReplTool::new().working_dir(working_dir())),
            ToolSource::Todos => {
                let list = todos.expect("todo list is created whenever the recipe asks for todos");
                Arc::new(TodoWriteTool::new(list.clone()))
            }
        });
    }
    tools.extend(definition.shared_tools.iter().cloned());
    tools
}
