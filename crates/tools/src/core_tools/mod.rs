//! Default host, process, and workspace tools shipped with the harness.
//!
//! This module owns the implementations and wiring returned by
//! [`core_tools`]. Optional workflow tools and the shell-less filesystem
//! administration bundle remain separate at the crate root.

mod files;
mod glob;
pub(crate) mod iogate;
mod multi_edit_spec;
mod mutation_preflight;
mod mutations;
mod patch_format;
pub(crate) mod pgroup;
mod process;
mod search;
pub(crate) mod shell;
mod stats;
pub(crate) mod workspace;

use std::sync::Arc;

use orca_harness_core::Tool;

pub use files::{EditFileTool, FileGuard, ListDirTool, ReadFileTool, WriteFileTool};
pub use glob::GlobTool;
pub use mutation_preflight::MutationPreflight;
pub use mutations::{ApplyPatchTool, MultiEditTool};
pub use process::{ProcessNotification, ProcessNotificationKind, ProcessTool};
pub use search::GrepTool;
pub use shell::{Executor, ShellTool};
pub use stats::{BackgroundProcess, BackgroundStats};
pub use workspace::Workspace;

/// The recommended default tool set: a local `shell` and `process`
/// (persistent sessions / background processes), plus file
/// read/write/edit/patch/multi-edit/list, `grep`, and `glob`, all rooted at
/// `ws`. Returned as trait objects ready for
/// [`Agent::tool_arc`](orca_harness_core::Agent).
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
        Arc::new(ApplyPatchTool::new(ws.clone()).guard(guard.clone())),
        Arc::new(MultiEditTool::new(ws.clone()).guard(guard.clone())),
        Arc::new(ListDirTool::new(ws.clone())),
        Arc::new(GrepTool::new(ws.clone())),
        Arc::new(GlobTool::new(ws.clone())),
    ]
}
