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
    /// Agent-level tool state ([`FileGuard`] and [`TodoList`]) persists
    /// across calls until [`Session::clear`](crate::Session::clear).
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

    pub fn todo_list(&self) -> Option<TodoList> {
        self.inner.todo_list.clone()
    }
}

impl RetryConfig {
    pub(crate) fn duration(&self) -> Duration {
        Duration::from_millis(self.backoff_ms)
    }
}
