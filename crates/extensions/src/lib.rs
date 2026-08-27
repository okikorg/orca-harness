//! # Orca Harness Extensions
//!
//! The critical extensions that make the harness independently useful and
//! give hosts a rich seam to build on, without touching the kernel:
//!
//! - [`EventStream`] — typed lifecycle events to a sink (the main
//!   observability/integration seam). Tags mirror the platform NDJSON
//!   union so a sidecar can serialize them directly.
//! - [`ToolPolicy`] — allow/deny tool calls before execution.
//! - [`Truncation`] — cap oversized tool outputs to protect the context
//!   window and downstream line caps. Pair with a [`TruncationStore`] and
//!   the [`ReadToolResultTool`] so the model can page through the full
//!   original of anything that was trimmed.
//! - [`ToolRetry`] / [`RetryModel`] — retry failing tools (an extension)
//!   and transient model errors (a `Model` decorator).
//! - [`UsageMeter`] — accumulate self-reported token usage across a run.
//! - [`SessionHandler`] — record the transcript to an append-only JSONL
//!   file and resume it later; durable memory as an Extension concern.
//! - [`LongSession`] — compact context against dynamically discovered model
//!   capacity so an interactive session can keep running without a model table.
//! - [`MemoryExtension`] and [`MemoryModel`] — retrieve explicitly saved global
//!   and workspace memories into a transient user-authority model request.
//!
//! Each subscribes only to the hooks it needs, so registering one it does
//! not use costs nothing on the kernel's hot path.
//!
//! ```no_run
//! use orca_harness_core::Agent;
//! use orca_harness_extensions::{EventStream, ToolPolicy, Truncation, UsageMeter};
//!
//! # async fn example(model: impl orca_harness_core::Model) -> Result<(), Box<dyn std::error::Error>> {
//! let (meter, usage) = UsageMeter::new();
//! let (events, mut rx) = EventStream::channel();
//! let agent = Agent::new(model)
//!     .extension(events)
//!     .extension(ToolPolicy::new().deny(["rm_rf"]))
//!     .extension(Truncation::default())
//!     .extension(meter);
//! let answer = agent.run("do the thing").await?;
//! while let Ok(event) = rx.try_recv() { /* serialize event */ }
//! let _ = (answer, usage.total());
//! # Ok(()) }
//! ```

mod compact;
mod events;
mod long_session;
mod memory;
mod policy;
mod read_tool_result;
mod retry;
mod session;
mod truncation;
mod usage;

pub use compact::{compact, CompactConfig, CompactError, CompactReport};
pub use events::{EventSink, EventStream, HarnessEvent};
pub use long_session::{ContextCapacity, LongSession, LongSessionConfig};
pub use memory::{
    MemoryError, MemoryExtension, MemoryManageTool, MemoryModel, MemoryRecord, MemoryScope,
    MemorySearchTool, MemoryStore, MEMORY_GUIDANCE, MEMORY_MANAGE_TOOL, MEMORY_SEARCH_TOOL,
};
pub use policy::{PolicyOutcome, PolicyRule, ToolPolicy};
pub use read_tool_result::ReadToolResultTool;
pub use retry::{RetryModel, ToolRetry};
pub use session::{
    new_session_id, workspace_key, LoadedSession, SessionError, SessionFile, SessionHandler,
    SessionMeta, SESSION_FORMAT_VERSION,
};
pub use truncation::{Truncation, TruncationStore};
pub use usage::{UsageHandle, UsageMeter};
