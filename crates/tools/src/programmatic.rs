//! Programmatic tool calling: the capability that lets code running in
//! `bun_repl` call the agent's other tools.
//!
//! The capability is a [`ToolDispatch`] handle, built per run by the host
//! and handed only to `bun_repl` (see [`BunReplTool::with_dispatch`]).
//! Nested calls go through the kernel's own [`Dispatcher`], so policy
//! (`before_tool` deny and rewrite), `around_tool`, `after_tool`,
//! `tool_finished` and `tool_result` hooks, unknown-tool errors and
//! interruption reporting are exactly those of a top-level call.
//!
//! The handle's registry never holds `bun_repl`, so nested code cannot
//! reenter or reset the interpreter that is running it. Each nested
//! batch is its own dispatch: it shares no keys, locks or permits with
//! the parent's batch, so a parent waiting on its nested calls can never
//! hold something those calls wait for.
//!
//! [`BunReplTool::with_dispatch`]: crate::BunReplTool::with_dispatch

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::Value;

use orca_harness_core::{
    Dispatcher, Extension, ExtensionRegistry, HarnessError, Tool, ToolCall, ToolContext,
    ToolRegistry, ToolResult, ToolSchema,
};

/// Decides whether a tool is visible to programmatic callers. It is read
/// live, before every catalog request and every nested batch.
pub type ToolVisibility = Arc<dyn Fn(&ToolSchema) -> bool + Send + Sync>;

/// Host configuration for programmatic tool calling.
#[derive(Clone, Default)]
pub struct ProgrammaticTools {
    visibility: Option<ToolVisibility>,
}

impl std::fmt::Debug for ProgrammaticTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgrammaticTools").finish_non_exhaustive()
    }
}

impl ProgrammaticTools {
    pub fn new() -> Self {
        Self::default()
    }

    /// Restrict nested calls to the tools `visibility` accepts. A hidden
    /// tool is absent from the catalog and fails like an unknown tool.
    /// The host shares its deferred discovery state here, so nested code
    /// sees what the model sees.
    pub fn visibility(mut self, visibility: ToolVisibility) -> Self {
        self.visibility = Some(visibility);
        self
    }

    /// Whether nested code may see and call `schema`'s tool.
    pub fn visible(&self, schema: &ToolSchema) -> bool {
        self.visibility
            .as_ref()
            .is_none_or(|visible| visible(schema))
    }
}

/// The capability to dispatch nested tool calls for one run. Clones share
/// the run's tools, extensions and call id sequence.
#[derive(Clone)]
pub struct ToolDispatch {
    tools: Vec<Arc<dyn Tool>>,
    extensions: ExtensionRegistry,
    config: ProgrammaticTools,
    max_parallel: usize,
    sequence: Arc<AtomicU64>,
}

impl ToolDispatch {
    /// A handle over the run's `tools` and `extensions`, in registration
    /// order. Pass the run's own extension instances so nested calls share
    /// their state with the run. `bun_repl` is dropped from `tools`.
    pub fn new(
        tools: impl IntoIterator<Item = Arc<dyn Tool>>,
        extensions: impl IntoIterator<Item = Arc<dyn Extension>>,
        config: ProgrammaticTools,
    ) -> Self {
        let mut registry = ExtensionRegistry::new();
        for extension in extensions {
            registry.register(extension);
        }
        Self {
            tools: tools
                .into_iter()
                .filter(|tool| tool.schema().name != crate::bun_repl::NAME)
                .collect(),
            extensions: registry,
            config,
            max_parallel: usize::MAX,
            sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Cap concurrently executing calls within one nested batch; pass the
    /// run's `max_parallel_tools`. Unbounded by default, as in the kernel.
    pub fn max_parallel(mut self, max_parallel: usize) -> Self {
        self.max_parallel = max_parallel;
        self
    }

    /// The schemas nested code may call, in registration order.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.registry().schemas()
    }

    /// Dispatch `calls` (name and arguments) on behalf of the call `parent`
    /// describes, through the kernel dispatcher. Nested call ids are
    /// `{parent}.ptc{n}`, numbered across the run. Cancellation and the run
    /// deadline come from `parent`; dropping the returned future aborts
    /// every nested call still running and cancels nothing else.
    #[allow(clippy::result_large_err)] // HarnessError is large crate-wide
    pub async fn execute(
        &self,
        parent: &ToolContext,
        calls: Vec<(String, Value)>,
    ) -> Result<Vec<ToolResult>, HarnessError> {
        let calls = calls
            .into_iter()
            .map(|(name, arguments)| ToolCall {
                id: format!(
                    "{}.ptc{}",
                    parent.call_id,
                    self.sequence.fetch_add(1, Ordering::Relaxed) + 1
                ),
                name,
                arguments,
            })
            .collect();
        Dispatcher::new()
            .execute(
                calls,
                &self.registry(),
                &self.extensions,
                &parent.cancellation,
                parent.deadline,
                self.max_parallel,
            )
            .await
    }

    /// The visible tools right now. Rebuilt per request so a visibility
    /// change (a deferred tool loaded, an MCP tool selected) applies to
    /// the next nested batch.
    fn registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for tool in &self.tools {
            if self.config.visible(&tool.schema()) {
                registry.register(tool.clone());
            }
        }
        registry
    }
}
