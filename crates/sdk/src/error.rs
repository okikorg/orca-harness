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
    #[error("session already has an active run")]
    BusySession,
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("session has no persistent recorder")]
    EphemeralSession,
    #[error("cannot read image {path}: {source}")]
    Image {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
