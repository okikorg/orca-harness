//! # Orca Harness
//!
//! A minimal, high-performance agent execution kernel. The privileged path
//! stays tiny; capabilities are added through Extensions.
//!
//! > The loop is sacred. Everything around it is extensible.
//!
//! The kernel owns: agent configuration, context, model invocation, the
//! loop, tool dispatch, concurrent execution, cancellation, deadlines,
//! limits, call/result pairing, deterministic ordering, and the Extension
//! lifecycle. Everything else (memory, MCP, permissions, tracing, retries,
//! sandboxing, ...) composes through [`Extension`]s or lives in the host.
//!
//! ```no_run
//! use orca_harness_core::{Agent, FnTool};
//! use serde_json::json;
//!
//! # async fn example(model: impl orca_harness_core::Model) -> Result<(), orca_harness_core::HarnessError> {
//! let shell = FnTool::new("shell", "Run a command", json!({"type": "object"}), |input, _ctx| async move {
//!     Ok(input)
//! });
//!
//! let agent = Agent::new(model).tool(shell);
//! let result = agent.run("Fix the failing test").await?;
//! # Ok(()) }
//! ```

mod agent;
mod agent_loop;
mod context;
mod dispatcher;
mod error;
mod extension;
mod limits;
mod model;
mod tool;

pub mod testing;

pub use agent::Agent;
pub use context::{Context, Image, Message};
pub use dispatcher::Dispatcher;
pub use error::{ExtensionError, HarnessError, ModelError, ToolError};
pub use extension::{Extension, ExtensionRegistry, Next, Subscriptions, ToolDecision};
pub use limits::Limits;
pub use model::{DeltaSink, Model, ModelDelta, ModelResponse, Usage};
pub use tool::{
    Concurrency, FnTool, Tool, ToolCall, ToolContext, ToolName, ToolRegistry, ToolResult,
    ToolSchema,
};

// Cancellation is a kernel concern; re-export the primitive so hosts don't
// need a direct tokio-util dependency.
pub use tokio_util::sync::CancellationToken;
