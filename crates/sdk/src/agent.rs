use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::{Extension, Limits, Model, Tool};
use orca_harness_extensions::{LongSessionConfig, MemoryModel, RetryModel, ToolPolicy};
use orca_harness_tools::{BunReplTool, FileGuard, PyKernelTool, TodoList, TodoWriteTool};

use crate::extensions::{Compaction, ExtensionConfig, RetryConfig, TruncationConfig};
use crate::tools::{preset_tools, ToolPreset};
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
    pub tools: Vec<Arc<dyn Tool>>,
    pub extensions: Vec<Arc<dyn Extension>>,
    /// The `skill` tool is registered, so each run pairs it with
    /// `SkillOnce`. Not in `extensions`: that list is registered ahead
    /// of compaction, and `SkillOnce` has to run after it.
    pub skill_once: bool,
    pub extension_config: ExtensionConfig,
    pub context_capacity: Option<u64>,
    pub file_guard: FileGuard,
    pub todo_list: Option<TodoList>,
}

pub struct AgentBuilder {
    harness: Harness,
    model: Arc<dyn Model>,
    model_name: String,
    system_prompt: Option<String>,
    limits: Limits,
    preset: ToolPreset,
    tools: Vec<Arc<dyn Tool>>,
    extensions: Vec<Arc<dyn Extension>>,
    extension_config: ExtensionConfig,
    context_capacity: Option<u64>,
    file_guard: FileGuard,
    todo_list: Option<TodoList>,
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
            tools: Vec::new(),
            extensions: Vec::new(),
            extension_config: ExtensionConfig::default(),
            context_capacity: None,
            file_guard: FileGuard::new(),
            todo_list: None,
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

    pub fn tool(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    pub fn tool_arc(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
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

    pub fn python(mut self) -> Self {
        let dir = self.harness.workspace().root().display().to_string();
        self.tools
            .push(Arc::new(PyKernelTool::new().working_dir(dir)));
        self
    }

    pub fn bun(mut self) -> Self {
        let dir = self.harness.workspace().root().display().to_string();
        self.tools
            .push(Arc::new(BunReplTool::new().working_dir(dir)));
        self
    }

    pub fn todos(mut self) -> Self {
        let list = TodoList::new();
        self.tools.push(Arc::new(TodoWriteTool::new(list.clone())));
        self.todo_list = Some(list);
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
        let custom_tools = std::mem::take(&mut self.tools);
        self.tools = preset_tools(self.preset, self.harness.workspace(), &self.file_guard);
        self.tools.extend(custom_tools);
        let mut skill_once = false;
        if let Some(skills) = &self.skills {
            if let Some(tool) = skills.tool() {
                self.tools.push(tool);
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
            self.tools.extend(mcp.tools());
            self.model = Arc::new(orca_harness_tool_extensions::mcp::McpModel::new(
                self.model,
                mcp.catalog(),
            ));
        }
        if let Some(memory) = &self.memory {
            if memory.search_tool {
                self.tools
                    .push(Arc::new(orca_harness_extensions::MemorySearchTool::new(
                        memory.memory.store().clone(),
                        memory.memory.scope().clone(),
                    )));
            }
            if memory.manage_tool {
                self.tools
                    .push(Arc::new(orca_harness_extensions::MemoryManageTool::new(
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
                tools: self.tools,
                extensions: self.extensions,
                skill_once,
                extension_config: self.extension_config,
                context_capacity: self.context_capacity,
                file_guard: self.file_guard,
                todo_list: self.todo_list,
            }),
        })
    }
}

impl Agent {
    pub fn new_session(&self) -> SessionBuilder {
        SessionBuilder::new(self.clone())
    }

    /// Run one request in a fresh ephemeral session and return its result.
    ///
    /// This is the one-shot entry point: it is exactly
    /// `self.new_session().ephemeral().open()?.run(request).await` with the
    /// session dropped when the future completes, whether it succeeded or
    /// failed. There is no separate execution path.
    ///
    /// Each call is a fresh conversation. No history carries between calls;
    /// use a [`Session`](crate::Session) for multi-turn work.
    ///
    /// The ephemeral session owns every session-scoped resource for the
    /// duration of the call and is dropped when it returns. Today that
    /// session-scoped state is the conversation context and the truncation
    /// store, so after `run` returns neither is reachable; the result's
    /// `messages` is the only record.
    ///
    /// Detached work started during the run (background subagents,
    /// processes, workflows) has no session owner yet: those handles live in
    /// agent-level tool state, so this method neither cancels nor awaits
    /// them, and nothing returned here can observe them. Later phases make
    /// that work session-owned; from then on dropping the ephemeral session
    /// cancels it best-effort and it still cannot be observed or awaited
    /// afterwards. Either way, hosts that need detached work to outlive a
    /// run, or to be reported on, must open a `Session` explicitly and use
    /// its lifecycle methods.
    ///
    /// Agent-level shared tool state is not reset by this method: the file
    /// guard and the todo list are owned by the `Agent` in the current
    /// design and persist across calls until `Session::clear` is invoked on
    /// some session of this agent.
    ///
    /// Overlapping calls are allowed because each has its own session; this
    /// differs from [`Session::run`](crate::Session::run), which rejects an
    /// overlapping run with [`SdkError::BusySession`].
    ///
    /// ```rust,no_run
    /// use orca_harness_sdk::{AnthropicModel, Harness};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let harness = Harness::builder().workspace(".").build()?;
    /// let model = AnthropicModel::new("claude-haiku-4-5").api_key("sk-ant-...");
    /// let agent = harness.agent(model).build()?;
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

    pub fn todo_list(&self) -> Option<TodoList> {
        self.inner.todo_list.clone()
    }
}

impl RetryConfig {
    pub(crate) fn duration(&self) -> Duration {
        Duration::from_millis(self.backoff_ms)
    }
}
