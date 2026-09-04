//! # Orca Harness Tools
//!
//! The core set of tools that make the harness independently useful: run
//! commands on the host (or a target machine / container), keep
//! long-lived processes and interactive sessions alive across calls, and
//! read, write, patch, batch-edit, list, glob, and search files. Four workflow tools
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
mod core_tools;
mod fs_admin;
mod kernel;
mod subagent;
mod todo;

use std::sync::Arc;

use orca_harness_core::Tool;

pub(crate) use core_tools::{iogate, pgroup, shell, workspace};

pub use ask::{
    AskAnswer, AskOption, AskQuestion, AskRequest, AskResponse, AskTool, AskTopic, AskTopicAnswer,
    MAX_ASK_TOPICS,
};
pub use bun_repl::BunReplTool;
pub use core_tools::{
    core_tools, core_tools_with_executor, core_tools_with_guard, ApplyPatchTool, BackgroundStats,
    EditFileTool, Executor, FileGuard, GlobTool, GrepTool, ListDirTool, MultiEditTool,
    MutationPreflight, ProcessNotification, ProcessNotificationKind, ProcessTool, ReadFileTool,
    ShellTool, Workspace, WriteFileTool,
};
pub use fs_admin::{CopyFileTool, CreateFolderTool, DeleteFileTool, FileInfoTool, RenameFileTool};
pub use kernel::PyKernelTool;
pub use subagent::{
    SpawnExtensions, SubagentDepth, SubagentIdentity, SubagentModel, SubagentSpawn, SubagentTool,
    AUTO_SUBAGENT_ROUTE, DEFAULT_SUBAGENT_MAX_STEPS, DEFAULT_SUBAGENT_TIMEOUT, MAX_SUBAGENT_DEPTH,
    MAX_SUBAGENT_MAX_STEPS, MAX_SUBAGENT_OUTPUT_CHARS, MAX_SUBAGENT_RETRY_ATTEMPTS,
    MAX_SUBAGENT_RETRY_BACKOFF_MS, MAX_SUBAGENT_TIMEOUT_SECS, MIN_SUBAGENT_DEPTH,
    MIN_SUBAGENT_MAX_STEPS, MIN_SUBAGENT_OUTPUT_CHARS, MIN_SUBAGENT_TIMEOUT_SECS,
    PREFERENCE_SUBAGENT_ROUTE,
};
pub use todo::{TodoItem, TodoList, TodoStatus, TodoWriteTool};

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
