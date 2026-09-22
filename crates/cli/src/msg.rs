//! Channel types wiring the agent worker, the approval extension, and the
//! terminal UI together.

use orca_harness_core::{CancellationToken, Image};
use orca_harness_extensions::{CompactReport, HarnessEvent};
use orca_harness_model_providers::openrouter::ModelInfo;
use orca_harness_tools::{AskRequest, ProcessNotification};
use tokio::sync::oneshot;

/// A selectable endpoint preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenRouter,
    Vercel,
    CheaperInference,
    OpenAi,
    OpenAiCodex,
    Anthropic,
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAuth {
    ApiKey { environment: &'static str },
    OAuth,
    None,
}

impl Provider {
    pub const ALL: [Provider; 7] = [
        Provider::OpenRouter,
        Provider::Vercel,
        Provider::CheaperInference,
        Provider::OpenAi,
        Provider::OpenAiCodex,
        Provider::Anthropic,
        Provider::Local,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Provider::OpenRouter => "openrouter",
            Provider::Vercel => "vercel",
            Provider::CheaperInference => "cheaperinference",
            Provider::OpenAi => "openai",
            Provider::OpenAiCodex => "openai-codex",
            Provider::Anthropic => "anthropic",
            Provider::Local => "local",
        }
    }

    /// Parse a label as produced by [`Provider::label`].
    pub fn from_label(label: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.label() == label)
    }

    pub fn base_url(self) -> &'static str {
        match self {
            Provider::OpenRouter => orca_harness_model_providers::openrouter::OPENROUTER_BASE_URL,
            Provider::Vercel => orca_harness_model_providers::vercel::VERCEL_GATEWAY_BASE_URL,
            Provider::CheaperInference => {
                orca_harness_model_providers::cheaperinference::CHEAPERINFERENCE_BASE_URL
            }
            Provider::OpenAi => "https://api.openai.com/v1",
            Provider::OpenAiCodex => orca_harness_model_providers::openai_codex::CODEX_BASE_URL,
            Provider::Anthropic => orca_harness_model_providers::anthropic::ANTHROPIC_BASE_URL,
            Provider::Local => "http://localhost:11434/v1",
        }
    }

    pub fn auth(self) -> ProviderAuth {
        match self {
            Provider::OpenRouter => ProviderAuth::ApiKey {
                environment: "OPENROUTER_API_KEY",
            },
            Provider::Vercel => ProviderAuth::ApiKey {
                environment: "AI_GATEWAY_API_KEY",
            },
            Provider::CheaperInference => ProviderAuth::ApiKey {
                environment: "CHEAPERINFERENCE_API_KEY",
            },
            Provider::OpenAi => ProviderAuth::ApiKey {
                environment: "OPENAI_API_KEY",
            },
            Provider::OpenAiCodex => ProviderAuth::OAuth,
            Provider::Anthropic => ProviderAuth::ApiKey {
                environment: "ANTHROPIC_API_KEY",
            },
            Provider::Local => ProviderAuth::None,
        }
    }

    /// The environment variable holding this provider's key, if it needs one.
    pub fn key_env(self) -> Option<&'static str> {
        match self.auth() {
            ProviderAuth::ApiKey { environment } => Some(environment),
            ProviderAuth::OAuth | ProviderAuth::None => None,
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

    /// The model a session starts on when nothing is selected: no flag, no
    /// `ORCA_MODEL`, no saved choice. A static answer on purpose — asking the
    /// provider's catalog for one would put a network round trip in front of
    /// every cold start, and the picker and the window probe both refresh the
    /// real catalog in the background once the session is up.
    pub fn default_model(self) -> &'static str {
        match self {
            Provider::OpenRouter => "openrouter/auto",
            Provider::Vercel => "anthropic/claude-haiku-4.5",
            Provider::CheaperInference => "anthropic/claude-haiku-4.5",
            Provider::OpenAi => "gpt-4o-mini",
            Provider::OpenAiCodex => "gpt-5.4",
            Provider::Anthropic => "claude-haiku-4-5",
            Provider::Local => "qwen3.5:9b",
        }
    }

    pub fn supports_images(self) -> bool {
        matches!(
            self,
            Provider::OpenRouter
                | Provider::Vercel
                | Provider::CheaperInference
                | Provider::OpenAi
                | Provider::OpenAiCodex
                | Provider::Anthropic
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Provider, ProviderAuth};

    #[test]
    fn vercel_gateway_provider_has_expected_configuration() {
        assert_eq!(Provider::from_label("vercel"), Some(Provider::Vercel));
        assert_eq!(
            Provider::Vercel.base_url(),
            "https://ai-gateway.vercel.sh/v1"
        );
        assert_eq!(
            Provider::Vercel.auth(),
            ProviderAuth::ApiKey {
                environment: "AI_GATEWAY_API_KEY"
            }
        );
        assert!(Provider::Vercel.supports_images());
    }

    #[test]
    fn cheaperinference_provider_has_expected_configuration() {
        assert_eq!(
            Provider::from_label("cheaperinference"),
            Some(Provider::CheaperInference)
        );
        assert_eq!(
            Provider::CheaperInference.base_url(),
            "https://api.cheaperinference.com/v1"
        );
        assert_eq!(
            Provider::CheaperInference.auth(),
            ProviderAuth::ApiKey {
                environment: "CHEAPERINFERENCE_API_KEY"
            }
        );
        assert!(Provider::CheaperInference.supports_images());
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
    /// Restrict the prompt to a plain yes/no: `a`/`A` are ignored and
    /// never offered. For one-off decisions (e.g. /refine) where
    /// "always allow" is meaningless.
    pub yes_no: bool,
    pub respond: oneshot::Sender<ApprovalResponse>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunId {
    User(u64),
    BackgroundProcess {
        generation: u64,
        sequence: u64,
    },
    /// One hidden wake-up run delivering a batch of subagent completions.
    BackgroundSubagents {
        sequence: u64,
    },
}

/// Everything the UI task can receive.
pub enum UiMsg {
    Event(HarnessEvent),
    Approval(ApprovalRequest),
    /// Structured clarification requested by the running agent.
    Ask(AskRequest),
    /// The worker accepted a run. Background process context remains hidden
    /// from the transcript; only the model's response is user-facing.
    RunStarted {
        id: RunId,
        cancel: CancellationToken,
    },
    /// The worker finished a run: final answer or error text.
    RunDone {
        id: RunId,
        result: Result<String, String>,
    },
    /// A user-invoked `!` shell command finished. Its tool result has
    /// already been appended to the model context by the worker.
    ShellDone {
        id: RunId,
    },
    /// The endpoint's model catalog (already filtered), or the fetch error.
    Models {
        request_id: u64,
        result: Result<Vec<ModelInfo>, String>,
    },
    /// The worker switched the active model and optional reasoning effort.
    ModelChanged {
        id: String,
        reasoning_effort: Option<String>,
    },
    /// The worker compacted the conversation (or could not).
    Compacted(Result<CompactReport, String>),
    /// /refine finished: a validated single-skill proposal awaiting the
    /// user's accept/reject, or why no proposal survived.
    RefineDone(Box<Result<crate::refine::RefineOutcome, String>>),
    /// A one-line status notice for the transcript (session warnings,
    /// load failures).
    Notice(String),
    /// Visible even on the welcome screen, unlike transcript-only notices.
    McpConnecting(bool),
    /// The worker adopted a previously recorded session; the transcript
    /// is replayed into the UI.
    SessionLoaded {
        id: String,
        messages: Vec<orca_harness_core::Message>,
    },
    /// /clear preserved the previous transcript and began a fresh session.
    SessionCleared {
        id: Option<String>,
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
    /// A subagent's resolved runtime identity, emitted before its first model
    /// step so the live rail can name the worker immediately.
    SubagentStarted {
        id: u64,
        parent_id: Option<u64>,
        depth: u32,
        call_id: String,
        task: String,
        identity: Option<orca_harness_tools::SubagentIdentity>,
        /// The workflow run this agent executes a stage of, and which stage.
        /// Both `None` for an ordinary subagent.
        run: Option<u64>,
        stage: Option<String>,
    },
    /// Reconcile detached jobs that terminate before entering the agent loop.
    SubagentCompleted {
        id: u64,
        is_error: bool,
        message: String,
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
        id: RunId,
        prompt: String,
        images: Vec<Image>,
        cancel: CancellationToken,
    },
    /// Run a user-entered `!` command as a synthetic shell tool call and
    /// retain the paired call/result in model-visible context.
    Shell {
        id: RunId,
        command: String,
        working_dir: String,
        cancel: CancellationToken,
    },
    /// A detached process reached a requested lifecycle condition. Generation
    /// filtering happens in the context-owning worker before any model wake.
    BackgroundProcess {
        generation: u64,
        sequence: u64,
        notification: ProcessNotification,
    },
    /// A detached subagent completed into the completion inbox. A parent
    /// mid-run reads the inbox at its next model call; this wake starts a
    /// hidden run for an idle parent, and is a no-op once the inbox is
    /// empty.
    BackgroundSubagentsReady,
    /// Start a session-persistent sidekick through the registered subagent tool.
    SidekickStart {
        task: String,
        tier: Option<String>,
    },
    /// Release one selected session-persistent sidekick.
    SidekickStop {
        spawn_id: u64,
    },
    /// Reset the conversation to just the system prompt.
    Clear,
    /// Deterministically compact the conversation in place.
    Compact,
    /// Review the trajectory and propose one skill (see crate::refine).
    /// The worker then asks for approval through the standard gate and
    /// applies the skill itself on a yes.
    Refine,
    /// Delete the most recently applied /refine skill.
    RefineUndo,
    /// Drop the last `turns` user turns from the conversation and from
    /// the recorded session, so the conversation continues from an
    /// earlier point.
    Rewind {
        turns: usize,
    },
    /// Branch: continue this conversation in a new session file, leaving
    /// the current file exactly where it was.
    Fork,
    /// Adopt a recorded session: replace the context and record there.
    LoadSession {
        path: std::path::PathBuf,
    },
    /// Fetch the endpoint's model catalog, keeping ids containing `filter`.
    ListModels {
        request_id: u64,
        filter: String,
    },
    ListSubagentModels {
        request_id: u64,
        provider: Provider,
    },
    SetSubagentModel {
        tier: String,
        provider: Provider,
        model: String,
    },
    /// Run a provider-owned interactive OAuth flow, then activate it.
    LoginProvider {
        provider: Provider,
    },
    /// Result of a detached provider login; `attempt` rejects stale completions.
    LoginFinished {
        provider: Provider,
        attempt: u64,
        result: Result<(), String>,
    },
    /// Switch the active model and reasoning effort for subsequent runs
    /// (context is kept).
    SetModel {
        id: String,
        reasoning_effort: Option<String>,
    },
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
    /// Background reconciliation completed; publish diagnostics and rebuild
    /// at a worker command boundary, never in the middle of an agent turn.
    McpReloaded {
        notices: Vec<String>,
    },
    /// Rescan the skill directories and rebuild the agent so the
    /// catalog the model sees matches what is on disk (the conversation
    /// context is kept).
    ReloadSkills,
    /// Execute a bounded Agent Plugin MCP probe without blocking the TUI.
    TestPlugin {
        path: std::path::PathBuf,
    },
    /// Copy skills in from a folder or a repository, then rescan and
    /// rebuild. Runs in the worker because cloning is slow and must not
    /// block the interface.
    InstallSkill {
        source: String,
        here: bool,
    },
}
