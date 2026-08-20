//! Channel types wiring the agent worker, the approval extension, and the
//! terminal UI together.

use orca_harness_core::CancellationToken;
use orca_harness_extensions::HarnessEvent;
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
    /// The endpoint's model catalog (already filtered), or the fetch error.
    Models(Result<Vec<ModelInfo>, String>),
    /// The worker switched the active model to this id.
    ModelChanged(String),
    /// The worker switched provider and reset the model to its default.
    ProviderChanged {
        provider: &'static str,
        model: String,
    },
}

/// Commands the UI sends to the agent worker.
pub enum WorkerCmd {
    Run {
        prompt: String,
        cancel: CancellationToken,
    },
    /// Reset the conversation to just the system prompt.
    Clear,
    /// Fetch the endpoint's model catalog, keeping ids containing `filter`.
    ListModels { filter: String },
    /// Switch the active model for subsequent runs (context is kept).
    SetModel { id: String },
    /// Switch endpoint provider; `api_key` overrides env detection.
    SetProvider {
        provider: Provider,
        api_key: Option<String>,
    },
}
