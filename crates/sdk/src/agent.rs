use std::sync::Arc;

use orca_harness_core::{Extension, Limits, Model, Tool};
use orca_harness_extensions::{LongSessionConfig, MemoryModel, ToolPolicy};
use orca_harness_tools::{FileGuard, ProgrammaticTools};

use crate::background::{ProcessConfig, SubagentConfig};
use crate::extensions::{
    Compaction, ExtensionConfig, ModelRetryOptions, ToolRetryOptions, TruncationConfig,
};
use crate::tools::{ToolPreset, ToolSource};
use crate::{Harness, Mcp, MemoryConfig, RunRequest, RunResult, SdkError, SessionBuilder, Skills};

#[derive(Clone)]
pub struct Agent {
    pub(crate) inner: Arc<AgentDefinition>,
}

pub(crate) struct AgentDefinition {
    pub harness: Harness,
    pub model: Arc<dyn Model>,
    pub model_name: String,
    pub system_prompt: Option<String>,
    pub limits: Limits,
    /// The tool recipe: sessions materialize `preset` and `tool_sources`
    /// themselves (see [`crate::tools::SessionTools`]) so mutable
    /// built-ins are session-owned; the `skill` tool and the MCP tools
    /// are read off `skills` and `mcp` at each run boundary (see
    /// [`crate::tools::RunTools`]); `memory_tools` are built once here
    /// and appended after them.
    pub preset: ToolPreset,
    pub tool_sources: Vec<ToolSource>,
    pub skills: Option<Skills>,
    pub mcp: Option<Mcp>,
    pub memory_tools: Vec<Arc<dyn Tool>>,
    pub extensions: Vec<Arc<dyn Extension>>,
    pub extension_config: ExtensionConfig,
    pub context_capacity: Option<u64>,
    /// Caller-owned instances that every session uses instead of creating
    /// its own. `None` means each session gets a fresh one.
    pub shared_file_guard: Option<FileGuard>,
    /// Sessions build their own subagent manager, inbox, and `subagent`
    /// tool from this recipe (see [`crate::background::BackgroundServices`]).
    pub subagents: Option<SubagentConfig>,
    /// How sessions of a [`ToolPreset::Coding`] agent build their
    /// `shell` and `process` tools; `None` means local defaults.
    pub processes: Option<ProcessConfig>,
    /// Lets `bun_repl` code call the run's other tools; see
    /// [`AgentBuilder::programmatic_tools`].
    pub programmatic_tools: Option<ProgrammaticTools>,
}

pub struct AgentBuilder {
    harness: Harness,
    model: Arc<dyn Model>,
    model_name: String,
    system_prompt: Option<String>,
    limits: Limits,
    preset: ToolPreset,
    tool_sources: Vec<ToolSource>,
    extensions: Vec<Arc<dyn Extension>>,
    extension_config: ExtensionConfig,
    context_capacity: Option<u64>,
    shared_file_guard: Option<FileGuard>,
    subagents: Option<SubagentConfig>,
    processes: Option<ProcessConfig>,
    mcp: Option<Mcp>,
    skills: Option<Skills>,
    memory: Option<MemoryConfig>,
    programmatic_tools: Option<ProgrammaticTools>,
}

impl AgentBuilder {
    pub(crate) fn new(harness: Harness, model: Arc<dyn Model>) -> Self {
        Self {
            harness,
            model,
            model_name: "custom".into(),
            system_prompt: None,
            limits: Limits::default(),
            preset: ToolPreset::None,
            tool_sources: Vec::new(),
            extensions: Vec::new(),
            extension_config: ExtensionConfig::default(),
            context_capacity: None,
            shared_file_guard: None,
            subagents: None,
            processes: None,
            mcp: None,
            skills: None,
            memory: None,
            programmatic_tools: None,
        }
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.model_name = name.into();
        self
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn tools(mut self, preset: ToolPreset) -> Self {
        self.preset = preset;
        self
    }

    /// Register a custom tool. Custom tools are caller-owned: every
    /// session opened from the agent shares this one instance.
    pub fn tool(self, tool: impl Tool + 'static) -> Self {
        self.tool_arc(Arc::new(tool))
    }

    /// Register a shared custom tool instance; see [`AgentBuilder::tool`].
    pub fn tool_arc(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tool_sources.push(ToolSource::Custom(tool));
        self
    }

    /// Use one caller-owned read-before-write guard for every session
    /// instead of giving each session its own (and
    /// [`Session::clear`](crate::Session::clear) clears it).
    pub fn file_guard(mut self, guard: FileGuard) -> Self {
        self.shared_file_guard = Some(guard);
        self
    }

    pub fn extension(mut self, extension: impl Extension + 'static) -> Self {
        self.extensions.push(Arc::new(extension));
        self
    }

    pub fn extension_arc(mut self, extension: Arc<dyn Extension>) -> Self {
        self.extensions.push(extension);
        self
    }

    pub fn policy(self, policy: ToolPolicy) -> Self {
        self.extension(policy)
    }

    pub fn events(mut self, enabled: bool) -> Self {
        self.extension_config.events = enabled;
        self
    }

    pub fn usage(mut self, enabled: bool) -> Self {
        self.extension_config.usage = enabled;
        self
    }

    pub fn truncation(mut self, config: TruncationConfig) -> Self {
        self.extension_config.truncation = Some(config);
        self
    }

    pub fn truncation_off(mut self) -> Self {
        self.extension_config.truncation = None;
        self
    }

    /// Retry the agent's own tool calls in every run; see
    /// [`ToolRetryOptions`] for what is retried, what is excluded, and how
    /// it interacts with subagents that retry inside.
    ///
    /// A plain [`RetryConfig`](crate::RetryConfig) converts with the
    /// built-in classifiers on, so `tool_retry(RetryConfig::attempts(n))`
    /// no longer replays every error: native file mutation errors are
    /// excluded and `shell` / `process` / `web_fetch` data failures are
    /// retried. The older retry-every-`Err`-only behaviour is
    /// `ToolRetryOptions::attempts(n).retry_data_failures(false).exclude_non_idempotent(false)`.
    pub fn tool_retry(mut self, options: impl Into<ToolRetryOptions>) -> Self {
        self.extension_config.retry = Some(options.into());
        self
    }

    /// Retry transient model failures; see [`ModelRetryOptions`]. The
    /// model is wrapped once at build, shared by every session and every
    /// subagent inheriting it, so retry is never nested. A plain
    /// [`RetryConfig`](crate::RetryConfig) converts to a fixed attempt
    /// cap with its backoff.
    pub fn model_retry(mut self, options: impl Into<ModelRetryOptions>) -> Self {
        self.extension_config.model_retry = Some(options.into());
        self
    }

    pub fn compaction(mut self, compaction: Compaction) -> Self {
        self.extension_config.compaction = compaction;
        self
    }

    pub fn context_capacity(mut self, tokens: u64) -> Self {
        self.context_capacity = Some(tokens);
        self
    }

    pub fn on_compact(
        mut self,
        callback: impl Fn(orca_harness_extensions::CompactReport) + Send + Sync + 'static,
    ) -> Self {
        self.extension_config.on_compact = Some(Arc::new(callback));
        self
    }

    pub fn automatic_compaction(self, compact_at_percent: u8, tail_percent: u8) -> Self {
        self.compaction(Compaction::Automatic(LongSessionConfig {
            compact_at_percent,
            tail_percent,
        }))
    }

    /// Add the Python kernel tool. Each session starts its own kernel.
    pub fn python(mut self) -> Self {
        self.tool_sources.push(ToolSource::Python);
        self
    }

    /// Add the Bun REPL tool. Each session starts its own REPL.
    pub fn bun(mut self) -> Self {
        self.tool_sources.push(ToolSource::Bun);
        self
    }

    /// Let code in the session's `bun_repl` call the run's other tools
    /// through `await tools.list()`, `tools.call(name, arguments)` and
    /// `tools.batch([...])`. Each nested call runs through the run's
    /// dispatcher policy and extensions, under the id `{parent}.ptc{n}`.
    /// Nested code sees what the model sees (MCP server tools only once
    /// selected) narrowed by `config`'s visibility, and never `bun_repl`
    /// itself. Takes effect in sessions with a `bun_repl` (see
    /// [`bun`](Self::bun)); sandboxed sessions also give each subagent
    /// child its own interpreter with the child's tools.
    pub fn programmatic_tools(mut self, config: ProgrammaticTools) -> Self {
        self.programmatic_tools = Some(config);
        self
    }

    /// Add the `subagent` tool and the typed host handle
    /// [`Session::subagents`](crate::Session::subagents). Each session owns
    /// its manager, queue, and completion inbox; only `config`'s live
    /// settings handle is shared between sessions. Workers get the agent's
    /// tool preset and custom tools, nothing else. Configuring subagents
    /// also registers the `workflow` tool and enables
    /// [`Session::workflows`](crate::Session::workflows) unless the
    /// config sets [`SubagentConfig::workflows`]`(false)`.
    pub fn subagents(mut self, config: SubagentConfig) -> Self {
        self.subagents = Some(config);
        self
    }

    /// Configure how sessions run `shell` and `process` commands: the
    /// executor and the process tool's limits. Only
    /// [`ToolPreset::Coding`] ships those tools, so
    /// [`build`](Self::build) rejects this with [`SdkError::Config`] for
    /// any other preset. Without it a `Coding` agent runs commands on the
    /// local host with default limits. Each session builds its own tools
    /// from the recipe; process events reach the host through
    /// [`Session::notifications`](crate::Session::notifications).
    pub fn processes(mut self, config: ProcessConfig) -> Self {
        self.processes = Some(config);
        self
    }

    pub fn memory(mut self, config: MemoryConfig) -> Self {
        self.memory = Some(config);
        self
    }

    /// Offer the MCP interface tools and every connected server's tools
    /// over `mcp`. The registry is read at each run boundary: a run
    /// captures the tool set when it starts and keeps it, so `connect`,
    /// `connect_stdio`, and `disconnect` reach the next run of every
    /// session and never a run in flight. A server disconnected or
    /// replaced mid-run stays callable through that run's captured tools
    /// (its calls go to the old connection: they succeed or fail with
    /// that connection's transport error). Schema visibility is the one
    /// thing read live, through the
    /// [`McpModel`](orca_harness_tool_extensions::mcp::McpModel) wrap
    /// built here: a disconnect unhides the run's captured schemas
    /// (the catalog no longer knows them), while a replace hides them
    /// until the new catalog tool is selected. See [`Mcp`] for the
    /// selection rule.
    pub fn mcp(mut self, mcp: Mcp) -> Self {
        self.mcp = Some(mcp);
        self
    }

    /// Offer the `skill` tool over `skills`. The catalog is read at each
    /// run boundary, so `enable`, `disable`, and `reload` reach the next
    /// run of every session and never a run in flight; `scaffold` and
    /// `install` change disk and reach the catalog at the next `reload`,
    /// and `uninstall` drops its entry from the catalog at once. A run
    /// offers no `skill` tool when no skill is enabled at its start.
    pub fn skills(mut self, skills: Skills) -> Self {
        self.skills = Some(skills);
        self
    }

    pub fn build(mut self) -> Result<Agent, SdkError> {
        if self.processes.is_some() && self.preset != ToolPreset::Coding {
            return Err(SdkError::Config(format!(
                "process configuration requires ToolPreset::Coding, not {:?}",
                self.preset
            )));
        }
        let mut memory_tools: Vec<Arc<dyn Tool>> = Vec::new();
        if let Some(config) = &self.extension_config.model_retry {
            self.model = Arc::new(config.wrap(self.model));
        }
        if let Some(mcp) = &self.mcp {
            // The tools themselves are read per run (see `RunTools`); the
            // visibility wrap reads the catalog live, so it is built once.
            self.model = Arc::new(orca_harness_tool_extensions::mcp::McpModel::new(
                self.model,
                mcp.catalog(),
            ));
        }
        if let Some(subagents) = &self.subagents {
            subagents.apply_to_settings();
        }
        if let Some(memory) = &self.memory {
            if memory.search_tool {
                memory_tools.push(Arc::new(orca_harness_extensions::MemorySearchTool::new(
                    memory.memory.store().clone(),
                    memory.memory.scope().clone(),
                )));
            }
            if memory.manage_tool {
                memory_tools.push(Arc::new(orca_harness_extensions::MemoryManageTool::new(
                    memory.memory.store().clone(),
                    memory.memory.scope().clone(),
                )));
            }
            if memory.automatic_recall {
                let extension = memory.extension();
                self.model = Arc::new(MemoryModel::new(self.model, extension));
            }
        }
        Ok(Agent {
            inner: Arc::new(AgentDefinition {
                harness: self.harness,
                model: self.model,
                model_name: self.model_name,
                system_prompt: self.system_prompt,
                limits: self.limits,
                preset: self.preset,
                tool_sources: self.tool_sources,
                skills: self.skills,
                mcp: self.mcp,
                memory_tools,
                extensions: self.extensions,
                extension_config: self.extension_config,
                context_capacity: self.context_capacity,
                shared_file_guard: self.shared_file_guard,
                subagents: self.subagents,
                processes: self.processes,
                programmatic_tools: self.programmatic_tools,
            }),
        })
    }
}

impl Agent {
    pub fn new_session(&self) -> SessionBuilder {
        SessionBuilder::new(self.clone())
    }

    /// Run one request in a fresh ephemeral session and return its result.
    /// The session is dropped when the call returns, on success or error.
    ///
    /// Each call is a fresh conversation: no history carries between calls.
    /// Use a [`Session`](crate::Session) for multi-turn work.
    ///
    /// Overlapping calls are allowed because each has its own session,
    /// unlike [`Session::run`](crate::Session::run), which rejects overlap
    /// with [`SdkError::BusySession`].
    ///
    /// Detached work started during the run (background subagents,
    /// processes, workflows) is not awaited, and cannot be observed from
    /// the result: the ephemeral session is dropped when this method
    /// returns, which cancels that work on a best-effort basis (live
    /// subagent runs are cancelled, live process groups are killed).
    /// Open a `Session` to manage it instead.
    ///
    /// Built-in mutable tool state (the read-before-write [`FileGuard`],
    /// background processes, Python/Bun REPLs) is owned by
    /// the ephemeral session and released when the call returns. Only
    /// explicitly shared instances persist across calls:
    /// [`AgentBuilder::file_guard`] and
    /// custom tools registered with [`AgentBuilder::tool_arc`].
    ///
    /// ```rust,no_run
    /// # async fn example(agent: orca_harness_sdk::Agent) -> Result<(), orca_harness_sdk::SdkError> {
    /// let result = agent.run("Summarise the README in one line.").await?;
    /// println!("{}", result.text);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn run(&self, request: impl Into<RunRequest>) -> Result<RunResult, SdkError> {
        let session = self.new_session().ephemeral().open()?;
        session.run(request).await
    }

    /// Resume only when the supplied binding matches the persisted environment.
    /// The host reconnects or provisions the sandbox before calling this method.
    #[allow(clippy::result_large_err)] // SdkError is large crate-wide
    pub fn resume_session_with_environment(
        &self,
        id: &str,
        environment: crate::SessionEnvironment,
    ) -> Result<crate::Session, SdkError> {
        crate::Session::resume(self.clone(), id, environment)
    }

    pub fn resume_session(&self, id: &str) -> Result<crate::Session, SdkError> {
        crate::Session::resume(self.clone(), id, crate::SessionEnvironment::Local)
    }
}
