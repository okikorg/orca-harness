//! # Orca Harness Tools
//!
//! The core set of tools that make the harness independently useful: run
//! commands on the host (or a target machine / container), and read,
//! write, edit, list, and search files. These are ordinary [`Tool`]
//! implementations — nothing here is privileged; they register into an
//! [`Agent`](orca_harness_core::Agent) like any other tool.
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
mod search;
mod shell;
mod workspace;

use std::sync::Arc;

use orca_harness_core::Tool;

pub use files::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool};
pub use search::GrepTool;
pub use shell::{Executor, ShellTool};
pub use workspace::Workspace;

/// The recommended default tool set: a local `shell`, plus file
/// read/write/edit/list and `grep`, all rooted at `ws`. Returned as
/// trait objects ready for [`Agent::tool_arc`](orca_harness_core::Agent).
pub fn core_tools(ws: &Workspace) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ShellTool::local().working_dir(ws.root().to_string_lossy().into_owned())),
        Arc::new(ReadFileTool::new(ws.clone())),
        Arc::new(WriteFileTool::new(ws.clone())),
        Arc::new(EditFileTool::new(ws.clone())),
        Arc::new(ListDirTool::new(ws.clone())),
        Arc::new(GrepTool::new(ws.clone())),
    ]
}

/// Like [`core_tools`] but the `shell` tool targets another machine via
/// `executor` (e.g. `Executor::ssh(...)`). File tools still operate on the
/// local workspace — pair with a synced or mounted workspace, or drop them
/// if the target's filesystem is only reachable over the shell.
pub fn core_tools_with_executor(ws: &Workspace, executor: Executor) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ShellTool::new(executor)),
        Arc::new(ReadFileTool::new(ws.clone())),
        Arc::new(WriteFileTool::new(ws.clone())),
        Arc::new(EditFileTool::new(ws.clone())),
        Arc::new(ListDirTool::new(ws.clone())),
        Arc::new(GrepTool::new(ws.clone())),
    ]
}
