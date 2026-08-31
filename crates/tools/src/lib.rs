//! # Orca Harness Tools
//!
//! The core set of tools that make the harness independently useful: run
//! commands on the host (or a target machine / container), keep
//! long-lived processes and interactive sessions alive across calls, and
//! read, write, edit, list, glob, and search files. Four workflow tools
//! build on the same primitives: [`PyKernelTool`] (persistent Python
//! compute), [`BunReplTool`] (persistent JavaScript/TypeScript compute),
//! [`SubagentTool`] (spawn
//! independent in-process agents, with nesting governed by a shared
//! [`SubagentDepth`]), and [`TodoWriteTool`] (the agent's plan as
//! structured state, readable by the host through a shared
//! [`TodoList`]). A separate [`fs_admin_tools`] bundle adds
//! copy/rename/delete/mkdir/stat for shell-less restricted agents. These
//! are ordinary [`Tool`] implementations — nothing here is privileged;
//! they register into an [`Agent`](orca_harness_core::Agent) like any
//! other tool.
//!
//! ```no_run
//! use orca_harness_core::Agent;
//! use orca_harness_tools::{core_tools, Workspace};
//!
//! # async fn example(model: impl orca_harness_core::Model) -> Result<(), Box<dyn std::error::Error>> {
//! let ws = Workspace::current_dir()?;
//! let mut agent = Agent::new(model);
//! for tool in core_tools(&ws) {
//!     agent = agent.tool_arc(tool);
//! }
//! let answer = agent.run("Run the tests and fix any failure").await?;
//! # let _ = answer; Ok(()) }
//! ```
//!
//! Targeting another machine: build the [`ShellTool`] with an
//! [`Executor`] — `Executor::ssh("user@host")` or
//! `Executor::docker_exec("container")` — and the model drives that
//! machine through the same `shell` contract.

mod ask;
mod bun_repl;
mod files;
mod fs_admin;
mod glob;
mod iogate;
mod kernel;
mod pgroup;
mod process;
mod search;
mod shell;
mod stats;
mod subagent;
mod todo;
mod workspace;

use std::sync::Arc;

use orca_harness_core::Tool;

pub use ask::{
    AskAnswer, AskOption, AskQuestion, AskRequest, AskResponse, AskTool, AskTopic, AskTopicAnswer,
    MAX_ASK_TOPICS,
};
pub use bun_repl::BunReplTool;
pub use files::{EditFileTool, FileGuard, ListDirTool, ReadFileTool, WriteFileTool};
pub use fs_admin::{CopyFileTool, CreateFolderTool, DeleteFileTool, FileInfoTool, RenameFileTool};
pub use glob::GlobTool;
pub use kernel::PyKernelTool;
pub use process::ProcessTool;
pub use search::GrepTool;
pub use shell::{Executor, ShellTool};
pub use stats::BackgroundStats;
pub use subagent::{
    SpawnExtensions, SubagentDepth, SubagentIdentity, SubagentModel, SubagentSpawn, SubagentTool,
    AUTO_SUBAGENT_ROUTE, DEFAULT_SUBAGENT_MAX_STEPS, DEFAULT_SUBAGENT_TIMEOUT, MAX_SUBAGENT_DEPTH,
    MAX_SUBAGENT_MAX_STEPS, MAX_SUBAGENT_OUTPUT_CHARS, MAX_SUBAGENT_RETRY_ATTEMPTS,
    MAX_SUBAGENT_RETRY_BACKOFF_MS, MAX_SUBAGENT_TIMEOUT_SECS, MIN_SUBAGENT_DEPTH,
    MIN_SUBAGENT_MAX_STEPS, MIN_SUBAGENT_OUTPUT_CHARS, MIN_SUBAGENT_TIMEOUT_SECS,
    PREFERENCE_SUBAGENT_ROUTE,
};
pub use todo::{TodoItem, TodoList, TodoStatus, TodoWriteTool};
pub use workspace::Workspace;

/// The recommended default tool set: a local `shell` and `process`
/// (persistent sessions / background processes), plus file
/// read/write/edit/list, `grep`, and `glob`, all rooted at `ws`. Returned
/// as trait objects ready for [`Agent::tool_arc`](orca_harness_core::Agent).
///
/// The file tools share a fresh [`FileGuard`], so `write_file` may only
/// overwrite a file this set has read. A host that rebuilds its tool set
/// while a conversation continues wants [`core_tools_with_guard`]
/// instead, so what the model read does not go with the old tools.
pub fn core_tools(ws: &Workspace) -> Vec<Arc<dyn Tool>> {
    core_tools_with_guard(ws, &FileGuard::new())
}

/// [`core_tools`] with a caller-owned [`FileGuard`], for hosts that
/// rebuild the tool set mid-session (a model switch, a config reload)
/// and want read-before-write to span the whole conversation rather than
/// resetting with every rebuild.
pub fn core_tools_with_guard(ws: &Workspace, guard: &FileGuard) -> Vec<Arc<dyn Tool>> {
    let dir = ws.root().to_string_lossy().into_owned();
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ShellTool::local().working_dir(dir.clone())),
        Arc::new(ProcessTool::local().working_dir(dir)),
    ];
    tools.extend(file_tools(ws, guard));
    tools
}

/// Like [`core_tools`] but `shell` and `process` target another machine
/// via `executor` (e.g. `Executor::ssh(...)`). File tools still operate on
/// the local workspace — pair with a synced or mounted workspace, or drop
/// them if the target's filesystem is only reachable over the shell.
pub fn core_tools_with_executor(ws: &Workspace, executor: Executor) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ShellTool::new(executor.clone())),
        Arc::new(ProcessTool::new(executor)),
    ];
    tools.extend(file_tools(ws, &FileGuard::new()));
    tools
}

/// The file half of the core set, wired to one guard.
fn file_tools(ws: &Workspace, guard: &FileGuard) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFileTool::new(ws.clone()).guard(guard.clone())),
        Arc::new(WriteFileTool::new(ws.clone()).guard(guard.clone())),
        Arc::new(EditFileTool::new(ws.clone()).guard(guard.clone())),
        Arc::new(ListDirTool::new(ws.clone())),
        Arc::new(GrepTool::new(ws.clone())),
        Arc::new(GlobTool::new(ws.clone())),
    ]
}

/// Filesystem administration bundle: `copy_file`, `rename_file`,
/// `delete_file`, `create_folder`, `file_info`. Not part of
/// [`core_tools`] — `shell` covers all of it on a full host. Register
/// these for shell-less restricted agents, gated by a `ToolPolicy`.
pub fn fs_admin_tools(ws: &Workspace) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(CopyFileTool::new(ws.clone())),
        Arc::new(RenameFileTool::new(ws.clone())),
        Arc::new(DeleteFileTool::new(ws.clone())),
        Arc::new(CreateFolderTool::new(ws.clone())),
        Arc::new(FileInfoTool::new(ws.clone())),
    ]
}
