//! Default host, process, and workspace tools shipped with the harness.
//!
//! This module owns the implementations and wiring returned by
//! [`core_tools`]. Optional workflow tools and the shell-less filesystem
//! administration bundle remain separate at the crate root.

mod edit;
mod edit_spec;
mod files;
mod glob;
pub(crate) mod iogate;
mod mutation_preflight;
pub(crate) mod pgroup;
mod process;
mod search;
pub(crate) mod shell;
mod stats;
pub(crate) mod workspace;

use std::sync::Arc;

use orca_harness_core::Tool;

pub use edit::EditFileTool;
pub use files::{FileGuard, ReadFileTool, WriteFileTool};
pub use glob::GlobTool;
pub use mutation_preflight::MutationPreflight;
pub use process::{
    ProcessController, ProcessEntry, ProcessNotification, ProcessNotificationKind, ProcessSnapshot,
    ProcessSpawn, ProcessTool, ProcessWrite,
};
pub use search::GrepTool;
pub use shell::{Executor, ShellTool};
pub use stats::{BackgroundProcess, BackgroundStats};
pub use workspace::Workspace;

/// The recommended default tool set: a local `shell` and `process`
/// (persistent sessions / background processes), plus file
/// read/write/edit, `grep`, and `glob`, all rooted at
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
    core_tools_with_process(ws, guard, ProcessTool::local().working_dir(dir))
}

/// [`core_tools_with_guard`] around a caller-built `process` tool, for a
/// host that configures the tool once (stats, notifications, limits) and
/// keeps its [`ProcessController`](ProcessTool::controller) instead of
/// replacing a default tool after the fact. The `shell` is local, rooted
/// at the workspace. Same order: `shell`, the given `process`, then the
/// file tools.
pub fn core_tools_with_process(
    ws: &Workspace,
    guard: &FileGuard,
    process: ProcessTool,
) -> Vec<Arc<dyn Tool>> {
    let dir = ws.root().to_string_lossy().into_owned();
    core_tools_with_shell_and_process(ws, guard, ShellTool::local().working_dir(dir), process)
}

/// The core set around caller-built `shell` and `process` tools, for a
/// host that configures both once (an executor, limits, notifications,
/// stats) and takes the process tool's controller before handing it
/// over. Same order as [`core_tools`]: `shell`, `process`, then the file
/// tools. The file tools always operate on the local workspace, whatever
/// executor the two command tools were built with: pair a remote
/// executor with a synced or mounted workspace, or drop the file tools
/// if the target's filesystem is only reachable over the shell.
pub fn core_tools_with_shell_and_process(
    ws: &Workspace,
    guard: &FileGuard,
    shell: ShellTool,
    process: ProcessTool,
) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(shell), Arc::new(process)];
    tools.extend(file_tools(ws, guard));
    tools
}

/// The core set for an agent running **inside** `sandbox`: a sandboxed
/// `shell` plus file tools rooted at `workspace_dir` *within that
/// sandbox*. Nothing in the returned set touches this machine.
///
/// This is the only supported way to build a sandboxed tool set. Pairing
/// a sandbox executor with a host-rooted [`Workspace`] by hand produces an
/// agent whose commands run inside and whose files are written outside —
/// a boundary with a hole in it, and the exact failure this function
/// exists to make unavailable.
///
/// Fails closed rather than degrading:
///
/// - a provider without `file_api` cannot back the file tools, so no set
///   is returned at all instead of one silently rooted on the host.
///
/// `process` is **not** included: it manages long-lived background
/// processes and has no sandbox backend yet, and registering its
/// host-backed version would put a local process inside a set that is
/// supposed to have none.
///
/// `bun_repl` and `py_kernel` are not part of the core set on any backend
/// — hosts add them deliberately. To keep them inside the boundary, build
/// them with [`BunReplTool::sandbox`](crate::BunReplTool::sandbox) and
/// [`PyKernelTool::sandbox`](crate::PyKernelTool::sandbox) and pass the
/// same sandbox. Both require
/// [`Capabilities::sessions`](orca_harness_core::Capabilities).
pub fn core_tools_in_sandbox(
    sandbox: Arc<dyn orca_harness_core::Sandbox>,
    workspace_dir: impl Into<std::path::PathBuf>,
) -> Result<Vec<Arc<dyn Tool>>, orca_harness_core::SandboxError> {
    if !sandbox.capabilities().file_api {
        return Err(orca_harness_core::SandboxError::Unsupported {
            provider: "this sandbox",
            capability: "a file API, which the file tools require",
        });
    }

    let dir = workspace_dir.into();
    let ws = Workspace::sandboxed(dir.clone(), sandbox.clone());
    let shell = ShellTool::new(Executor::sandbox(sandbox)).working_dir(dir.to_string_lossy());

    let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(shell)];
    tools.extend(file_tools(&ws, &FileGuard::new()));
    Ok(tools)
}

/// Like [`core_tools`] but `shell` and `process` target another machine
/// via `executor` (e.g. `Executor::ssh(...)`). File tools still operate on
/// the local workspace — pair with a synced or mounted workspace, or drop
/// them if the target's filesystem is only reachable over the shell.
pub fn core_tools_with_executor(ws: &Workspace, executor: Executor) -> Vec<Arc<dyn Tool>> {
    // A sandbox executor beside host-rooted file tools is the straddled
    // boundary `core_tools_in_sandbox` exists to prevent. Caught in debug
    // rather than ignored; the enforcement layer refuses it outright.
    debug_assert!(
        !(executor.is_sandboxed() && !ws.is_sandboxed()),
        "sandboxed shell with host file tools: use core_tools_in_sandbox"
    );
    core_tools_with_shell_and_process(
        ws,
        &FileGuard::new(),
        ShellTool::new(executor.clone()),
        ProcessTool::new(executor),
    )
}

/// The file half of the core set, wired to one guard.
fn file_tools(ws: &Workspace, guard: &FileGuard) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFileTool::new(ws.clone()).guard(guard.clone())),
        Arc::new(WriteFileTool::new(ws.clone()).guard(guard.clone())),
        Arc::new(EditFileTool::new(ws.clone()).guard(guard.clone())),
        Arc::new(GrepTool::new(ws.clone())),
        Arc::new(GlobTool::new(ws.clone())),
    ]
}
