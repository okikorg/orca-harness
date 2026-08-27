//! High-level, CLI-independent Rust facade for Orca Harness.

mod agent;
mod error;
mod extensions;
mod harness;
mod mcp;
mod memory;
mod run;
mod session;
mod skills;
mod tools;

pub use agent::{Agent, AgentBuilder};
pub use error::SdkError;
pub use extensions::{CompactCallback, Compaction, RetryConfig, TruncationConfig};
pub use harness::{Harness, HarnessBuilder};
pub use mcp::{Mcp, McpServerStatus};
pub use memory::{Memory, MemoryConfig};
pub use run::{EventCallback, RunHandle, RunRequest, RunResult};
pub use session::{Session, SessionBuilder, SessionMode, Sessions};
pub use skills::{SkillDestination, Skills};
pub use tools::ToolPreset;

pub use orca_harness_core::{
    CancellationToken, Context, Extension, FnTool, Image, Limits, Message, Model, Tool, Usage,
};
pub use orca_harness_extensions::{
    CompactConfig, CompactReport, HarnessEvent, LongSessionConfig, MemoryRecord, PolicyOutcome,
    PolicyRule, SessionFile, ToolPolicy,
};
pub use orca_harness_model_providers::{OpenAiCodexModel, OpenAiModel, OpenRouterModel};
pub use orca_harness_provider_auth::{BearerCredential, CredentialSource, StaticCredential};
pub use orca_harness_tool_extensions::web;
pub use orca_harness_tools::{
    core_tools, core_tools_with_executor, core_tools_with_guard, fs_admin_tools, AskTool,
    BackgroundStats, BunReplTool, Executor, FileGuard, PyKernelTool, SubagentDepth, SubagentTool,
    TodoList, TodoWriteTool, Workspace,
};
