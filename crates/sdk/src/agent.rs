use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::{Extension, Limits, Model, Tool};
use orca_harness_extensions::{LongSessionConfig, MemoryModel, RetryModel, ToolPolicy};
use orca_harness_tools::{FileGuard, TodoList};

use crate::background::{ProcessConfig, SubagentConfig};
use crate::extensions::{Compaction, ExtensionConfig, RetryConfig, TruncationConfig};
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
    /// built-ins are session-owned; `shared_tools` (skills, MCP, memory)
    /// are built once here and appended after them.
    pub preset: ToolPreset,
    pub tool_sources: Vec<ToolSource>,
    pub shared_tools: Vec<Arc<dyn Tool>>,
    pub extensions: Vec<Arc<dyn Extension>>,
    /// The `skill` tool is registered, so each run pairs it with
    /// `SkillOnce`. Not in `extensions`: that list is registered ahead
    /// of compaction, and `SkillOnce` has to run after it.
    pub skill_once: bool,
    pub extension_config: ExtensionConfig,
    pub context_capacity: Option<u64>,
    /// Caller-owned instances that every session uses instead of creating
    /// its own. `None` means each session gets a fresh one.
    pub shared_file_guard: Option<FileGuard>,
    pub shared_todo_list: Option<TodoList>,
    /// Sessions build their own subagent manager, inbox, and `subagent`
    /// tool from this recipe (see [`crate::background::BackgroundServices`]).
    pub subagents: Option<SubagentConfig>,
    /// How sessions of a [`ToolPreset::Coding`] agent build their
    /// `shell` and `process` tools; `None` means local defaults.
    pub processes: Option<ProcessConfig>,
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
    shared_todo_list: Option<TodoList>,
    subagents: Option<SubagentConfig>,
    processes: Option<ProcessConfig>,
    mcp: Option<Mcp>,
    skills: Option<Skills>,
    memory: Option<MemoryConfig>,
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
            shared_todo_list: None,
            subagents: None,
            processes: None,
            mcp: None,
            skills: None,
            memory: None,
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

    pub fn tool_retry(mut self, config: RetryConfig) -> Self {
        self.extension_config.retry = Some(config);
        self
    }

    pub fn model_retry(mut self, config: RetryConfig) -> Self {
        self.extension_config.model_retry = Some(config);
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

    /// Add the `todo_write` tool. Each session gets its own list, readable
    /// through [`Session::todo_list`](crate::Session::todo_list).
    pub fn todos(mut self) -> Self {
        if !self.has_todos() {
            self.tool_sources.push(ToolSource::Todos);
        }
        self
    }

    /// Add the `todo_write` tool backed by one caller-owned `list` that
    /// every session shares (and [`Session::clear`](crate::Session::clear)
    /// clears).
    pub fn todos_shared(mut self, list: TodoList) -> Self {
        self.shared_todo_list = Some(list);
        self.todos()
    }

    fn has_todos(&self) -> bool {
        self.tool_sources
            .iter()
            .any(|source| matches!(source, ToolSource::Todos))
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

    pub fn mcp(mut self, mcp: Mcp) -> Self {
        self.mcp = Some(mcp);
        self
    }

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
        let mut shared_tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut skill_once = false;
        if let Some(skills) = &self.skills {
            if let Some(tool) = skills.tool() {
                shared_tools.push(tool);
                skill_once = true;
            }
        }
        if let Some(config) = self.extension_config.model_retry {
            self.model = Arc::new(
                RetryModel::new(self.model, config.attempts)
                    .backoff(config.duration())
                    .retry_delay(orca_harness_model_providers::http_error::retry_delay),
            );
        }
        if let Some(mcp) = &self.mcp {
            shared_tools.extend(mcp.tools());
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
                shared_tools.push(Arc::new(orca_harness_extensions::MemorySearchTool::new(
                    memory.memory.store().clone(),
                    memory.memory.scope().clone(),
                )));
            }
            if memory.manage_tool {
                shared_tools.push(Arc::new(orca_harness_extensions::MemoryManageTool::new(
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
                shared_tools,
                extensions: self.extensions,
                skill_once,
                extension_config: self.extension_config,
                context_capacity: self.context_capacity,
                shared_file_guard: self.shared_file_guard,
                shared_todo_list: self.shared_todo_list,
                subagents: self.subagents,
                processes: self.processes,
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
    /// processes, workflows) is neither cancelled nor awaited by this method
    /// and cannot be observed from the result. Open a `Session` to manage it.
    ///
    /// Built-in mutable tool state (the read-before-write [`FileGuard`],
    /// the [`TodoList`], background processes, Python/Bun REPLs) is owned by
    /// the ephemeral session and released when the call returns. Only
    /// explicitly shared instances persist across calls:
    /// [`AgentBuilder::file_guard`], [`AgentBuilder::todos_shared`], and
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

    pub fn resume_session(&self, id: &str) -> Result<crate::Session, SdkError> {
        crate::Session::resume(self.clone(), id)
    }

    /// The caller-owned todo list configured with
    /// [`AgentBuilder::todos_shared`], if any. Lists created by
    /// [`AgentBuilder::todos`] are session-owned, so this returns `None`
    /// for them; read those through
    /// [`Session::todo_list`](crate::Session::todo_list).
    #[deprecated(
        since = "0.6.3",
        note = "todo state is session-owned; use Session::todo_list, or AgentBuilder::todos_shared to keep one caller-owned list"
    )]
    pub fn todo_list(&self) -> Option<TodoList> {
        self.inner.shared_todo_list.clone()
    }
}

impl RetryConfig {
    pub(crate) fn duration(&self) -> Duration {
        Duration::from_millis(self.backoff_ms)
    }
}
