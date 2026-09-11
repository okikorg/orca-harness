//! Tools exercised end-to-end through the Agent with a scripted model,
//! against a real temp workspace and the real host shell.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::time::timeout;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{Agent, CancellationToken, Message, ModelResponse, Tool, ToolContext};
use orca_harness_tools::{
    core_tools, core_tools_with_guard, core_tools_with_shell_and_process, BackgroundStats,
    CopyFileTool, CreateFolderTool, DeleteFileTool, EditFileTool, Executor, FileGuard,
    FileInfoTool, GlobTool, GrepTool, ListDirTool, MultiEditTool, ProcessEntry,
    ProcessNotificationKind, ProcessSpawn, ProcessTool, ProcessWrite, ReadFileTool, RenameFileTool,
    ShellTool, Workspace, WriteFileTool,
};

const RUN_TIMEOUT: Duration = Duration::from_secs(20);

static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn temp_ws() -> (Workspace, std::path::PathBuf) {
    // Unique dir under the system temp without external crates.
    let n = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("orca-harness-tools-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    (Workspace::new(dir.clone()), dir)
}

fn ctx() -> ToolContext {
    ToolContext {
        call_id: "t".into(),
        tool_name: "t".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

include!("tools/files_and_shell.rs");
include!("tools/glob_process_admin.rs");
include!("tools/process_host.rs");
