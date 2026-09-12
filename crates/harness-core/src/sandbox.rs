//! The Sandbox boundary. The kernel contains no provider-specific logic;
//! adapters (E2B, Daytona, Cloudflare, Vercel, local Docker, ...) live in
//! separate crates behind this trait, exactly as model adapters do behind
//! [`Model`](crate::Model).
//!
//! Two traits, deliberately separate. [`Sandbox`] is what a running tool
//! talks to; [`Provisioner`] is what creates one. Splitting them lets a
//! provider that owns the lifecycle itself (declare-an-environment-and-hand-
//! over, rather than create/exec/kill) implement only the half it offers.

use async_trait::async_trait;
use thiserror::Error;

/// What a provider can actually do. Reported per sandbox because it is a
/// property of the backend, not of the crate: of the four providers
/// reviewed, only two expose a stdin channel to a live process, and the
/// tools that need one cannot be assembled against the other two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// A long-lived process accepting stdin can be spawned. Required by
    /// `bun_repl`, `py_kernel` and `process`.
    pub sessions: bool,
    /// Files can be read and written without going through the shell.
    pub file_api: bool,
    /// Egress rules declared at creation are enforced by the provider.
    pub network_policy: bool,
}

/// One command to run inside the sandbox.
#[derive(Debug, Clone, Default)]
pub struct ExecRequest {
    /// The command line, interpreted by the sandbox's shell.
    pub command: String,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub timeout_ms: Option<u64>,
}

impl ExecRequest {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            ..Default::default()
        }
    }

    pub fn cwd(mut self, dir: impl Into<String>) -> Self {
        self.cwd = Some(dir.into());
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExecOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

/// A long-lived process to spawn inside the sandbox. Unlike [`ExecRequest`]
/// this is argv-style: the callers that need it (the REPL tools) build an
/// explicit argument vector rather than a shell string.
#[derive(Debug, Clone, Default)]
pub struct SpawnRequest {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
}

/// One chunk of output from a spawned process.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub stderr: bool,
    pub bytes: Vec<u8>,
}

/// A spawned process: stdin in, kill out. Output does **not** arrive
/// through a method here — [`Sandbox::spawn`] hands back a receiver
/// alongside the handle, so an implementation can pass its existing
/// channel through untouched instead of wrapping a receiver in a mutex
/// that would serialize every read.
#[async_trait]
pub trait Session: Send + Sync {
    async fn write_stdin(&self, bytes: &[u8]) -> Result<(), SandboxError>;
    async fn kill(&self) -> Result<(), SandboxError>;
}

/// The output channel returned with a [`Session`].
pub type Output = tokio::sync::mpsc::Receiver<Chunk>;

/// Whether a written file is executable. Nothing finer: the providers
/// disagree on mode handling and no tool needs more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    Regular,
    Executable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

#[async_trait]
pub trait Sandbox: Send + Sync {
    fn capabilities(&self) -> Capabilities;

    async fn exec(&self, request: ExecRequest) -> Result<ExecOutput, SandboxError>;

    /// Start a long-lived process. Returns the handle and its output
    /// stream. Providers reporting `sessions: false` return
    /// [`SandboxError::Unsupported`].
    async fn spawn(
        &self,
        request: SpawnRequest,
    ) -> Result<(Box<dyn Session>, Output), SandboxError>;

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError>;

    async fn write_file(
        &self,
        path: &str,
        bytes: &[u8],
        mode: FileMode,
    ) -> Result<(), SandboxError>;

    async fn list_dir(&self, path: &str) -> Result<Vec<Entry>, SandboxError>;

    async fn shutdown(&self) -> Result<(), SandboxError>;
}

/// Creates sandboxes. Separate from [`Sandbox`] so a provider that does not
/// own the lifecycle implements only what it offers.
#[async_trait]
pub trait Provisioner: Send + Sync {
    /// Capabilities the sandboxes this provisioner creates will report,
    /// known before one exists so tool assembly can refuse early.
    fn capabilities(&self) -> Capabilities;

    async fn start(&self) -> Result<std::sync::Arc<dyn Sandbox>, SandboxError>;
}

/// Failure from a sandbox provider. Mirrors the shape of
/// [`ModelError`](crate::ModelError): the kernel does not interpret these,
/// hosts and extensions decide what is recoverable.
#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("authentication failed: {0}")]
    Authentication(String),

    #[error("provisioning failed: {0}")]
    Provision(String),

    #[error("request failed: {0}")]
    Request(String),

    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// The provider does not offer this operation at all. Distinct from a
    /// failure: it is a fact about the backend, known in advance from
    /// [`Capabilities`], and the reason enclosure can refuse to start
    /// rather than fail on first use.
    #[error("{provider} does not support {capability}")]
    Unsupported {
        provider: &'static str,
        capability: &'static str,
    },

    #[error("sandbox terminated: {0}")]
    Terminated(String),

    #[error("timed out: {0}")]
    Timeout(String),
}
