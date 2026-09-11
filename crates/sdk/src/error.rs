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
    /// A typed background-process operation (see
    /// [`Processes`](crate::Processes)) was refused: the process is
    /// unknown or exited, the launch failed, the live process cap was
    /// reached, or the session's process manager is gone.
    #[error("process error: {0}")]
    Process(String),
    #[error("session already has an active run")]
    BusySession,
    /// The session was shut down (see
    /// [`Session::shutdown`](crate::Session::shutdown)); it admits no
    /// runs, spawns, or conversation changes afterwards.
    #[error("session is shut down")]
    SessionClosed,
    /// [`Session::shutdown`](crate::Session::shutdown) cancelled every
    /// detached worker and killed every background process, but
    /// `still_active` workers and `still_running_processes` processes
    /// had not exited when the grace period ran out. Worker cancellation
    /// is cooperative, so they may still be winding down; a process
    /// counted here survived its kill signal so far.
    #[error(
        "shutdown timed out with {still_active} background worker(s) and \
         {still_running_processes} process(es) still active"
    )]
    ShutdownTimeout {
        still_active: usize,
        still_running_processes: usize,
    },
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
