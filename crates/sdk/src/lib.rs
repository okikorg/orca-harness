//! High-level, CLI-independent Rust facade for Orca Harness.
//!
//! Everything a host needs is reachable from this crate alone. The most
//! common types live at the top level; the rest are grouped by purpose:
//!
//! - [`contracts`]: companion types for implementing `Model`, `Tool`, and
//!   `Extension`.
//! - [`providers`]: model adapters, the model catalog, and credentials
//!   (including Codex).
//! - [`orchestration`]: subagent, background process, and workflow handles.
//! - [`integrations`]: MCP, skills, web, memory, session recording, and the
//!   reusable extension handles (events, usage, retries, truncation).

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
    CancellationToken, Context, Extension, ExtensionError, FnTool, HarnessError, Image, Limits,
    Message, Model, ModelDelta, ModelError, ModelResponse, Subscriptions, Tool, ToolCall,
    ToolContext, ToolError, ToolResult, ToolSchema, Usage,
};
pub use orca_harness_extensions::{
    CompactConfig, CompactReport, HarnessEvent, LongSessionConfig, MemoryRecord, PolicyOutcome,
    PolicyRule, SessionFile, ToolPolicy,
};
pub use orca_harness_model_providers::openai_codex::{CodexCredential, CodexCredentialSource};
pub use orca_harness_model_providers::{
    AnthropicModel, OpenAiCodexModel, OpenAiModel, OpenRouterModel,
};
pub use orca_harness_provider_auth::{
    BearerCredential, CredentialError, CredentialSource, StaticCredential,
};
pub use orca_harness_tool_extensions::web;
pub use orca_harness_tools::{
    core_tools, core_tools_with_executor, core_tools_with_guard, fs_admin_tools, AskTool,
    BackgroundStats, BunReplTool, Executor, FileGuard, PyKernelTool, SubagentDepth, SubagentTool,
    TodoList, TodoWriteTool, Workspace,
};

/// Core contracts and every companion type needed to implement them.
pub mod contracts {
    pub use orca_harness_core::{
        CancellationToken, Concurrency, Context, DeltaSink, Extension, ExtensionError, FnTool,
        HarnessError, Image, Limits, Message, Model, ModelDelta, ModelError, ModelResponse, Next,
        Subscriptions, Tool, ToolCall, ToolContext, ToolDecision, ToolError, ToolName, ToolResult,
        ToolSchema, Usage,
    };
}

/// Model adapters, the model catalog, and provider credentials.
pub mod providers {
    pub use orca_harness_model_providers::openai_codex::{
        CodexCredential, CodexCredentialSource, CODEX_BASE_URL,
    };
    pub use orca_harness_model_providers::{
        AnthropicModel, ModelInfo, OpenAiCodexModel, OpenAiModel, OpenRouterModel, Pricing,
        ReasoningCapabilities, SupportedEfforts,
    };
    pub use orca_harness_provider_auth::{
        BearerCredential, CredentialError, CredentialErrorKind, CredentialSource, StaticCredential,
    };
}

/// Subagent, background process, and workflow handles for host orchestration.
pub mod orchestration {
    pub use orca_harness_tools::{
        BackgroundJob, BackgroundProcess, BackgroundStats, BackgroundStatus, ProcessNotification,
        ProcessNotificationKind, ProcessTool, SpawnExtensions, SubagentDepth, SubagentIdentity,
        SubagentManager, SubagentModel, SubagentNotification, SubagentSpawn, SubagentTool,
        WorkflowStore, WorkflowTool, DEFAULT_BACKGROUND_SUBAGENT_LIMIT, DEFAULT_SUBAGENT_MAX_STEPS,
        DEFAULT_SUBAGENT_TIMEOUT, MIN_SUBAGENT_DEPTH, MIN_SUBAGENT_MAX_STEPS,
    };
}

/// Optional integrations (MCP, skills, web, memory, session recording) and
/// the reusable extension handles a host can register directly.
pub mod integrations {
    pub use orca_harness_extensions::{
        EventSink, EventStream, HarnessEvent, LoadedSession, MemoryExtension, MemoryManageTool,
        MemoryModel, MemoryRecord, MemoryScope, MemorySearchTool, MemoryStore, ModelGate,
        ModelRetryConfig, RetryModel, SessionError, SessionFile, SessionHandler, SessionMeta,
        ToolExecutionEvents, ToolRetry, Truncation, TruncationStore, UsageHandle, UsageMeter,
    };
    pub use orca_harness_tool_extensions::{mcp, skills, web};
}
