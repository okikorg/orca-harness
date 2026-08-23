//! Channel types wiring the agent worker, the approval extension, and the
//! terminal UI together.

use orca_harness_core::CancellationToken;
use orca_harness_extensions::{CompactReport, HarnessEvent};
use orca_harness_model_openrouter::ModelInfo;
use tokio::sync::oneshot;

/// A selectable endpoint preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenRouter,
    OpenAi,
    Local,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::OpenRouter, Provider::OpenAi, Provider::Local];

    pub fn label(self) -> &'static str {
        match self {
            Provider::OpenRouter => "openrouter",
            Provider::OpenAi => "openai",
            Provider::Local => "local",
        }
    }

    /// Parse a label as produced by [`Provider::label`].
    pub fn from_label(label: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.label() == label)
    }

    pub fn base_url(self) -> &'static str {
        match self {
            Provider::OpenRouter => orca_harness_model_openrouter::OPENROUTER_BASE_URL,
            Provider::OpenAi => "https://api.openai.com/v1",
            Provider::Local => "http://localhost:11434/v1",
        }
    }

    /// The environment variable holding this provider's key, if it needs one.
    pub fn key_env(self) -> Option<&'static str> {
        match self {
            Provider::OpenRouter => Some("OPENROUTER_API_KEY"),
            Provider::OpenAi => Some("OPENAI_API_KEY"),
            Provider::Local => None,
        }
    }

    /// The key from the environment, ignoring blank values.
    pub fn env_key(self) -> Option<String> {
        self.key_env()
            .and_then(|env| std::env::var(env).ok())
            .filter(|key| !key.trim().is_empty())
    }

    /// The key saved to the config file by a previous session.
    pub fn stored_key(self) -> Option<String> {
        self.key_env()?;
        crate::config::stored_key(self.label())
    }

    /// The key a session would use without an explicit override:
    /// environment first, then the config file.
    pub fn resolve_key(self) -> Option<String> {
        self.env_key().or_else(|| self.stored_key())
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Provider::OpenRouter => "openrouter/auto",
            Provider::OpenAi => "gpt-4o-mini",
            Provider::Local => "qwen3.5:9b",
        }
    }
}

/// The user's answer to a tool approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalResponse {
    AllowOnce,
    /// Allow and stop asking for this tool for the rest of the session.
    AllowAlways,
    /// Like [`AllowAlways`](Self::AllowAlways), and the UI also saves the
    /// tool to this workspace's allowlist in the config file, so future
    /// sessions in the same workspace skip the prompt too.
    AllowAlwaysSave,
    Deny,
}

/// A tool call waiting on the user. Dropping `respond` denies the call.
pub struct ApprovalRequest {
    pub tool_name: String,
    /// Pre-rendered call line, e.g. `shell $ cargo test`.
    pub detail: String,
    pub respond: oneshot::Sender<ApprovalResponse>,
}

/// Everything the UI task can receive.
pub enum UiMsg {
    Event(HarnessEvent),
    Approval(ApprovalRequest),
    /// The worker finished a run: final answer or error text.
    RunDone(Result<String, String>),
    /// A user-invoked `!` shell command finished. Its tool result has
    /// already been appended to the model context by the worker.
    ShellDone,
    /// The endpoint's model catalog (already filtered), or the fetch error.
    Models(Result<Vec<ModelInfo>, String>),
    /// The worker switched the active model to this id.
    ModelChanged(String),
    /// The worker compacted the conversation (or could not).
    Compacted(Result<CompactReport, String>),
    /// A one-line status notice for the transcript (session warnings,
    /// load failures).
    Notice(String),
    /// The worker adopted a previously recorded session; the transcript
    /// is replayed into the UI.
    SessionLoaded {
        id: String,
        messages: Vec<orca_harness_core::Message>,
    },
    /// /clear emptied the current session file in place.
    SessionCleared {
        id: String,
    },
    /// /rewind dropped the tail of the conversation. The transcript is
    /// redrawn from `messages`; the session's cumulative token totals
    /// survive, because those tokens really were spent.
    ContextRewound {
        messages: Vec<orca_harness_core::Message>,
        notice: String,
    },
    /// /fork moved recording to a new session file. The conversation is
    /// unchanged, so the transcript stays as it is.
    SessionForked {
        id: String,
        parent: String,
    },
    /// The active model's context window, discovered from the endpoint.
    ContextWindow(Option<u64>),
    /// The worker switched provider and reset the model to its default.
    ProviderChanged {
        provider: Provider,
        model: String,
    },
    /// A lifecycle event from inside a running subagent (any depth).
    SubagentEvent {
        id: u64,
        parent_id: Option<u64>,
        depth: u32,
        /// The spawning agent's tool-call id (anchors the rail line).
        call_id: String,
        event: HarnessEvent,
    },
}

/// Commands the UI sends to the agent worker.
pub enum WorkerCmd {
    Run {
        prompt: String,
        cancel: CancellationToken,
    },
    /// Run a user-entered `!` command as a synthetic shell tool call and
    /// retain the paired call/result in model-visible context.
    Shell {
        command: String,
        working_dir: String,
        cancel: CancellationToken,
    },
    /// Reset the conversation to just the system prompt.
    Clear,
    /// Deterministically compact the conversation in place.
    Compact,
    /// Drop the last `turns` user turns from the conversation and from
    /// the recorded session, so the conversation continues from an
    /// earlier point.
    Rewind { turns: usize },
    /// Branch: continue this conversation in a new session file, leaving
    /// the current file exactly where it was.
    Fork,
    /// Adopt a recorded session: replace the context and record there.
    LoadSession { path: std::path::PathBuf },
    /// Fetch the endpoint's model catalog, keeping ids containing `filter`.
    ListModels { filter: String },
    /// Switch the active model for subsequent runs (context is kept).
    SetModel { id: String },
    /// Switch endpoint provider; `api_key` overrides env detection.
    SetProvider {
        provider: Provider,
        api_key: Option<String>,
    },
    /// Rebuild the agent so the extension toggles saved in the config
    /// apply to the next run (the conversation context is kept).
    ReloadExtensions,
    /// Reconnect the MCP servers saved in the config and rebuild the
    /// agent so their tools apply to the next run (the conversation
    /// context is kept).
    ReloadMcp,
    /// Rescan the skill directories and rebuild the agent so the
    /// catalog the model sees matches what is on disk (the conversation
    /// context is kept).
    ReloadSkills,
    /// Copy skills in from a folder or a repository, then rescan and
    /// rebuild. Runs in the worker because cloning is slow and must not
    /// block the interface.
    InstallSkill { source: String, here: bool },
}
