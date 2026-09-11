use orca_harness_extensions::{CompactReport, LongSessionConfig};

mod retry;

pub use retry::{ModelRetryOptions, ToolRetryOptions};

pub type CompactCallback = std::sync::Arc<dyn Fn(CompactReport) + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub struct TruncationConfig {
    pub max_string_chars: usize,
    pub store_budget_bytes: usize,
    pub expose_reader_tool: bool,
}

impl Default for TruncationConfig {
    fn default() -> Self {
        Self {
            max_string_chars: 16 * 1024,
            store_budget_bytes: 16 * 1024 * 1024,
            expose_reader_tool: true,
        }
    }
}

/// The simple retry shape: total attempts and a fixed backoff (250 ms
/// unless [`backoff_ms`](Self::backoff_ms) says otherwise). Accepted by
/// [`AgentBuilder::tool_retry`](crate::AgentBuilder::tool_retry),
/// [`AgentBuilder::model_retry`](crate::AgentBuilder::model_retry), and
/// [`SubagentConfig::tool_retry`](crate::SubagentConfig::tool_retry).
///
/// As a tool retry it converts to [`ToolRetryOptions`] with the built-in
/// classifiers on: native file mutation errors are not replayed, and
/// `shell` / `process` / `web_fetch` data failures are retried. Callers
/// that want the older retry-every-`Err`-only behaviour pass
/// `ToolRetryOptions::attempts(n).retry_data_failures(false).exclude_non_idempotent(false)`
/// instead. As a model retry it converts to [`ModelRetryOptions`] with a
/// fixed attempt cap and this backoff.
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    pub attempts: u32,
    pub backoff_ms: u64,
}

impl RetryConfig {
    pub fn attempts(attempts: u32) -> Self {
        Self {
            attempts: attempts.max(1),
            backoff_ms: 250,
        }
    }

    pub fn backoff_ms(mut self, backoff_ms: u64) -> Self {
        self.backoff_ms = backoff_ms;
        self
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub enum Compaction {
    #[default]
    Manual,
    Automatic(LongSessionConfig),
    Off,
}

#[derive(Clone)]
pub(crate) struct ExtensionConfig {
    pub events: bool,
    pub usage: bool,
    pub truncation: Option<TruncationConfig>,
    pub retry: Option<ToolRetryOptions>,
    pub model_retry: Option<ModelRetryOptions>,
    pub compaction: Compaction,
    pub on_compact: Option<CompactCallback>,
}

impl Default for ExtensionConfig {
    fn default() -> Self {
        Self {
            events: true,
            usage: true,
            truncation: Some(TruncationConfig::default()),
            retry: None,
            model_retry: None,
            compaction: Compaction::Manual,
            on_compact: None,
        }
    }
}
