use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Harness(#[from] orca_harness_core::HarnessError),
    #[error(transparent)]
    Session(#[from] orca_harness_extensions::SessionError),
    #[error(transparent)]
    Memory(#[from] orca_harness_extensions::MemoryError),
    #[error(transparent)]
    Mcp(#[from] orca_harness_tool_extensions::mcp::McpError),
    #[error("skill error: {0}")]
    Skill(String),
    /// A typed subagent operation (see [`Subagents`](crate::Subagents))
    /// was refused: routing, admission, or the worker itself failed.
    #[error("subagent error: {0}")]
    Subagent(String),
    #[error("session already has an active run")]
    BusySession,
    /// The session was shut down (see
    /// [`Session::shutdown`](crate::Session::shutdown)); it admits no
    /// runs, spawns, or conversation changes afterwards.
    #[error("session is shut down")]
    SessionClosed,
    /// [`Session::shutdown`](crate::Session::shutdown) cancelled every
    /// detached worker but `still_active` of them had not exited when the
    /// grace period ran out; cancellation is cooperative and they are
    /// still winding down.
    #[error("shutdown timed out with {still_active} background worker(s) still active")]
    ShutdownTimeout { still_active: usize },
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("session has no persistent recorder")]
    EphemeralSession,
    /// The conversation is not a shape the kernel can run from: an
    /// imported history (see
    /// [`SessionBuilder::context`](crate::SessionBuilder::context)) the
    /// kernel cannot resume, or a continuation (see
    /// [`RunRequest::continuation`](crate::RunRequest::continuation)) on
    /// a transcript with nothing to continue.
    #[error("invalid context: {0}")]
    InvalidContext(String),
    #[error("cannot read image {path}: {source}")]
    Image {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
