#![doc = include_str!("../README.md")]

mod agent;
mod background;
mod environment;
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
pub use background::{
    BackgroundNotification, ChildEventCallback, ProcessConfig, Processes, SubagentConfig,
    Subagents, Workflows, NOTIFICATION_CAPACITY,
};
pub use environment::SessionEnvironment;
pub use error::SdkError;
pub use extensions::{
    CompactCallback, Compaction, ModelRetryOptions, RetryConfig, ToolRetryOptions, TruncationConfig,
};
pub use harness::{Harness, HarnessBuilder};
pub use mcp::{Mcp, McpServerStatus};
pub use memory::{Memory, MemoryConfig};
pub use run::{
    EventCallback, RunEvent, RunHandle, RunInputMessage, RunOutcome, RunRequest, RunResult,
    DEFAULT_EVENT_CAPACITY,
};
pub use session::{Session, SessionBuilder, SessionMode, Sessions};
pub use skills::{SkillDestination, SkillPreview, SkillSourceOutcome, Skills};
pub use tools::ToolPreset;

pub use orca_harness_core::{
    current_tool_invocation, CancellationToken, Concurrency, Context, Extension, ExtensionError,
    FnTool, HarnessError, Image, Limits, Message, Model, ModelDelta, ModelError, ModelResponse,
    Next, ProgrammaticCall, ProgrammaticTools, Subscriptions, Tool, ToolCall, ToolContext,
    ToolDecision, ToolError, ToolInvocation, ToolResult, ToolSchema, Usage,
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
pub use orca_harness_tools::dag::{Kind, Stage};
pub use orca_harness_tools::{
    core_tools, core_tools_with_executor, core_tools_with_guard, fs_admin_tools, AskTool,
    BackgroundStats, BunReplTool, Executor, FileGuard, ProcessNotification,
    ProcessNotificationKind, PyKernelTool, SubagentDepth, SubagentTool, TodoList, TodoWriteTool,
    WorkflowSubmission, Workspace,
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

/// Sandbox adapters and the environment declaration hosts build them from.
///
/// Two axes, independently optional: an *enclosure* the agent runs inside
/// (substituting the backends of its own tools), and an *execution* target
/// it calls out to. A provider reports what it can actually do through
/// [`Capabilities`](sandbox::Capabilities) — notably whether a live process
/// accepts stdin, which two of the four hosted providers do not offer — so a
/// host can refuse to start rather than assemble a half-isolated tool set.
pub mod sandbox {
    pub use orca_harness_core::{
        Capabilities, Chunk, Entry, ExecOutput, ExecRequest, FileMode, Output, Provisioner,
        Sandbox, SandboxError, Session, SpawnRequest, Stat,
    };
    pub use orca_harness_sandbox_providers::{
        DockerProvisioner, EnvironmentSpec, Network, Packages, SetupCommand,
    };
}

/// Subagent, background process, and workflow handles for host orchestration.
///
/// The workflow vocabulary (`Stage`, `Kind`, `RunState`, `StageStatus`,
/// `GraphError`, `RunId`, `StageId`, `DEFAULT_STAGE_CAP`) is the
/// `harness-dag` engine's own, re-exported so hosts need no direct
/// dependency on it. A finished run's outcome
/// ([`WorkflowStatus::outcome`](orchestration::WorkflowStatus::outcome))
/// is the typed [`WorkflowOutcome`](orchestration::WorkflowOutcome), whose
/// JSON form is the document the parent receives.
pub mod orchestration {
    pub use orca_harness_tools::dag::{
        GraphError, Kind, RunId, RunState, Stage, StageId, StageStatus, DEFAULT_STAGE_CAP,
    };
    pub use orca_harness_tools::{
        core_tools_with_process, core_tools_with_shell_and_process, subagent_completions_prompt,
        ActiveInventory, BackgroundAcknowledgement, BackgroundJob, BackgroundProcess,
        BackgroundStats, BackgroundStatus, CompletionDelivery, CompletionInbox, ProcessController,
        ProcessEntry, ProcessSnapshot, ProcessSpawn, ProcessTool, ProcessWrite, SpawnExtensions,
        StageOutput, StageTiming, SubagentDepth, SubagentIdentity, SubagentManager, SubagentModel,
        SubagentNotification, SubagentOutcome, SubagentRequest, SubagentSpawn, SubagentTool,
        WorkflowAcknowledgement, WorkflowOutcome, WorkflowStageJob, WorkflowStatus, WorkflowStore,
        WorkflowSubmission, WorkflowTool, DEFAULT_COMPLETION_CAPACITY,
    };
}

/// Optional integrations (MCP, skills, web, memory, session recording) and
/// the reusable extension handles a host can register directly, with the
/// error types those handles return.
pub mod integrations {
    pub use orca_harness_extensions::{
        CompactConfig, CompactError, CompactReport, EventSink, EventStream, HarnessEvent,
        LoadedSession, LongSessionConfig, MemoryError, MemoryExtension, MemoryManageTool,
        MemoryModel, MemoryRecord, MemoryScope, MemorySearchTool, MemoryStore, ModelGate,
        ModelRetryConfig, PolicyOutcome, PolicyRule, RetryModel, SessionError, SessionFile,
        SessionHandler, SessionMeta, ToolExecutionEvents, ToolPolicy, ToolRetry, Truncation,
        TruncationStore, UsageHandle, UsageMeter,
    };
    pub use orca_harness_tool_extensions::{mcp, skills, web};
}
