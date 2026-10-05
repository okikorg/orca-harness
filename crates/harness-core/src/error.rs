//! Typed errors. The kernel defines failure identity and termination
//! semantics; Extensions may implement recovery policy on top.

use thiserror::Error;

use crate::Usage;

/// Terminal failure of an agent run.
///
/// Note that a *tool* failing at runtime is not terminal: the Dispatcher
/// normalizes tool execution failures into error-flagged [`crate::ToolResult`]s
/// that flow back to the model, which may recover. `HarnessError` is reserved
/// for conditions that end the run.
#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("model error: {0}")]
    Model(#[from] ModelError),

    #[error("tool error: {0}")]
    Tool(#[from] ToolError),

    #[error("extension error: {0}")]
    Extension(#[from] ExtensionError),

    #[error("cancelled")]
    Cancelled,

    #[error("deadline exceeded")]
    DeadlineExceeded,

    #[error("step limit exceeded")]
    StepLimitExceeded,

    #[error("invalid tool call: {0}")]
    InvalidToolCall(String),

    #[error("internal error: {0}")]
    Internal(String),
}

/// Failure reported by a [`crate::Model`] adapter.
#[derive(Debug, Error)]
pub enum ModelError {
    /// Authentication cannot recover without refreshing or user action.
    #[error("authentication failed: {0}")]
    Authentication(String),

    /// Transport-level failure (network, HTTP status, timeout).
    #[error("request failed: {0}")]
    Request(String),

    /// The provider answered but the payload could not be interpreted.
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// The provider stopped because its output limit was reached.
    #[error("output limit reached: {message}")]
    OutputLimit {
        message: String,
        usage: Option<Usage>,
    },

    /// The provider stopped generation because content was filtered.
    #[error("content filtered: {message}")]
    ContentFiltered {
        message: String,
        usage: Option<Usage>,
    },

    /// A streaming response ended without its completion marker.
    #[error("incomplete response: {message}")]
    IncompleteResponse {
        message: String,
        usage: Option<Usage>,
    },

    /// A completed response contained invalid JSON tool arguments.
    #[error("malformed arguments for tool {tool_name} ({argument_bytes} bytes): {message}")]
    MalformedToolArguments {
        tool_name: String,
        argument_bytes: usize,
        finish_reason: Option<String>,
        message: String,
        usage: Option<Usage>,
    },
}

/// Failure raised by a [`crate::Tool`] (or an `around_tool` wrapper).
#[derive(Debug, Error)]
#[error("{message}")]
pub struct ToolError {
    pub message: String,
}

impl ToolError {
    pub fn msg(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Failure raised by an [`crate::Extension`] hook. Extension failures in
/// deterministic hooks (`before_model`, `before_tool`, ...) are terminal:
/// an extension that wants to tolerate its own errors must catch them.
#[derive(Debug, Error)]
#[error("{extension}: {message}")]
pub struct ExtensionError {
    /// Name of the extension that failed.
    pub extension: String,
    pub message: String,
}

impl ExtensionError {
    pub fn new(extension: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            extension: extension.into(),
            message: message.into(),
        }
    }
}
