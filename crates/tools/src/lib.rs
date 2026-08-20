//! # Orca Harness Tools
//!
//! The core set of tools that make the harness independently useful: run
//! commands on the host (or a target machine / container), keep
//! long-lived processes and interactive sessions alive across calls, and
//! read, write, edit, list, glob, and search files. Two workflow tools
//! build on the same primitives: [`KernelTool`] (persistent Python
//! compute — state survives across calls) and [`SubagentTool`] (spawn
//! independent in-process agents, with nesting governed by a shared
//! [`SubagentDepth`]). A separate [`fs_admin_tools`] bundle adds
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

mod files;
mod fs_admin;
mod glob;
mod kernel;
mod pgroup;
mod process;
mod search;
mod shell;
mod stats;
mod subagent;
mod workspace;

use std::sync::Arc;

use orca_harness_core::Tool;

pub use files::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool};
pub use fs_admin::{CopyFileTool, CreateFolderTool, DeleteFileTool, FileInfoTool, RenameFileTool};
pub use glob::GlobTool;
pub use kernel::KernelTool;
pub use process::ProcessTool;
pub use search::GrepTool;
pub use shell::{Executor, ShellTool};
pub use stats::BackgroundStats;
pub use subagent::{SubagentDepth, SubagentTool, MAX_SUBAGENT_DEPTH, MIN_SUBAGENT_DEPTH};
pub use workspace::Workspace;

/// The recommended default tool set: a local `shell` and `process`
/// (persistent sessions / background processes), plus file
/// read/write/edit/list, `grep`, and `glob`, all rooted at `ws`. Returned
/// as trait objects ready for [`Agent::tool_arc`](orca_harness_core::Agent).
pub fn core_tools(ws: &Workspace) -> Vec<Arc<dyn Tool>> {
    let dir = ws.root().to_string_lossy().into_owned();
    vec![
        Arc::new(ShellTool::local().working_dir(dir.clone())),
        Arc::new(ProcessTool::local().working_dir(dir)),
        Arc::new(ReadFileTool::new(ws.clone())),
        Arc::new(WriteFileTool::new(ws.clone())),
        Arc::new(EditFileTool::new(ws.clone())),
        Arc::new(ListDirTool::new(ws.clone())),
        Arc::new(GrepTool::new(ws.clone())),
        Arc::new(GlobTool::new(ws.clone())),
    ]
}

/// Like [`core_tools`] but `shell` and `process` target another machine
/// via `executor` (e.g. `Executor::ssh(...)`). File tools still operate on
/// the local workspace — pair with a synced or mounted workspace, or drop
/// them if the target's filesystem is only reachable over the shell.
pub fn core_tools_with_executor(ws: &Workspace, executor: Executor) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ShellTool::new(executor.clone())),
        Arc::new(ProcessTool::new(executor)),
        Arc::new(ReadFileTool::new(ws.clone())),
        Arc::new(WriteFileTool::new(ws.clone())),
        Arc::new(EditFileTool::new(ws.clone())),
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
