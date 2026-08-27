use std::sync::Arc;

use orca_harness_core::Tool;
use orca_harness_tools::{
    core_tools_with_guard, fs_admin_tools, EditFileTool, FileGuard, GlobTool, GrepTool,
    ListDirTool, ReadFileTool, Workspace, WriteFileTool,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolPreset {
    #[default]
    None,
    ReadOnly,
    Coding,
    ShellLess,
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
