//! Tool presets and per-session tool construction. An agent records a
//! recipe (preset plus builder-ordered sources); each session materializes
//! it so mutable built-ins (file guard, todos, processes, REPLs) are owned
//! by that session alone.

use std::sync::Arc;

use orca_harness_core::Tool;
use orca_harness_tools::{
    core_tools_with_shell_and_process, fs_admin_tools, BunReplTool, EditFileTool, FileGuard,
    GlobTool, GrepTool, ListDirTool, ProcessController, ProcessTool, PyKernelTool, ReadFileTool,
    ShellTool, TodoList, TodoWriteTool, Workspace, WriteFileTool,
};
use tokio::sync::broadcast;

use crate::agent::AgentDefinition;
use crate::background::{
    BackgroundNotification, BackgroundServices, ProcessConfig, NOTIFICATION_CAPACITY,
};

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
/// no host handle over the process tool and no channel for its events.
pub(crate) fn preset_tools(
    preset: ToolPreset,
    workspace: &Workspace,
    guard: &FileGuard,
    processes: Option<&ProcessConfig>,
) -> Vec<Arc<dyn Tool>> {
    preset_tools_and_controller(preset, workspace, guard, processes, None).0
}

/// The preset's tools plus, for a preset that ships the `process` tool,
/// the typed controller over that same tool. The `shell` and `process`
/// tools are configured once from `processes` (local defaults without
/// it), the process tool reports to `events` when one is given, and the
/// controller is taken from the finished tool before registration, so
/// it and the model tool share one manager. The file tools are local
/// whatever the executor.
pub(crate) fn preset_tools_and_controller(
    preset: ToolPreset,
    workspace: &Workspace,
    guard: &FileGuard,
    processes: Option<&ProcessConfig>,
    events: Option<&broadcast::Sender<BackgroundNotification>>,
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
            let (shell, process) = command_tools(workspace, processes, events);
            let controller = process.controller();
            return (
                core_tools_with_shell_and_process(workspace, guard, shell, process),
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

/// The `shell` and `process` tools of a Coding session, built once from
/// the agent's process recipe. Both run on the configured executor (the
/// local shell by default) in the configured directory: the workspace
/// root for the local executor, none for a remote one unless the recipe
/// names it.
fn command_tools(
    workspace: &Workspace,
    config: Option<&ProcessConfig>,
    events: Option<&broadcast::Sender<BackgroundNotification>>,
) -> (ShellTool, ProcessTool) {
    let executor = config.and_then(|config| config.executor.clone());
    let working_dir = match config.and_then(|config| config.working_dir.as_ref()) {
        Some(dir) => Some(dir.display().to_string()),
        None if executor.is_none() => Some(workspace.root().display().to_string()),
        None => None,
    };
    let (mut shell, mut process) = match executor {
        Some(executor) => (ShellTool::new(executor.clone()), ProcessTool::new(executor)),
        None => (ShellTool::local(), ProcessTool::local()),
    };
    if let Some(dir) = working_dir {
        shell = shell.working_dir(dir.clone());
        process = process.working_dir(dir);
    }
    if let Some(config) = config {
        if let Some(bytes) = config.max_output_bytes {
            process = process.max_output_bytes(bytes);
        }
        if let Some(bytes) = config.buffer_cap {
            process = process.buffer_cap(bytes);
        }
        if let Some(n) = config.max_processes {
            process = process.max_processes(n);
        }
    }
    if let Some(events) = events {
        let events = events.clone();
        process = process.on_notification(move |notification| {
            // A session with no receiver simply drops the observation.
            let _ = events.send(BackgroundNotification::ProcessNotified(notification));
        });
    }
    (shell, process)
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
    /// The session's notification channel: subagent services and the
    /// process tool report here, hosts subscribe through
    /// [`Session::notifications`](crate::Session::notifications).
    pub(crate) events: broadcast::Sender<BackgroundNotification>,
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
        let (events, _) = broadcast::channel(NOTIFICATION_CAPACITY);
        let (mut tools, process) = preset_tools_and_controller(
            definition.preset,
            workspace,
            &file_guard,
            definition.processes.as_ref(),
            Some(&events),
        );
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
            .map(|config| BackgroundServices::new(definition, config, events.clone()));
        if let Some(services) = &background {
            tools.push(services.tool());
        }
        tools.extend(definition.shared_tools.iter().cloned());
        Self {
            file_guard,
            todo_list,
            background,
            process,
            events,
            tools,
        }
    }

    /// Reset every session-owned built-in: the guard, todos, detached
    /// subagents with their undelivered results, and background
    /// processes (killed and forgotten; the manager keeps serving).
    pub(crate) async fn clear(&self) {
        self.file_guard.clear();
        if let Some(todos) = &self.todo_list {
            todos.clear();
        }
        if let Some(background) = &self.background {
            background.clear();
        }
        if let Some(process) = &self.process {
            // Only fails once the manager is closed: nothing left to kill.
            let _ = process.kill_all().await;
        }
    }
}
