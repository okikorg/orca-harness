//! Session lifecycle: open/resume/fork/clear/reset, the session-owned tool
//! state, and the busy guard that serializes operations on one session.

mod execution;
mod persistence;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use orca_harness_core::{CancellationToken, Context, Tool};
use orca_harness_extensions::{
    compact, CompactConfig, CompactReport, SessionFile, SessionHandler, TruncationStore,
};
use orca_harness_tools::{FileGuard, TodoList};
use tokio::sync::Mutex;

use crate::tools::session_tools;
use crate::{Agent, RunHandle, RunRequest, RunResult, SdkError};

use execution::{execute, RunExecution};
use persistence::{load_session_store, save_session_store, session_store};

#[derive(Clone)]
pub struct Sessions {
    dir: PathBuf,
}

impl Sessions {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn list(&self) -> Vec<SessionFile> {
        SessionFile::list(&self.dir)
    }

    pub fn directory(&self) -> &Path {
        &self.dir
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionMode {
    #[default]
    Ephemeral,
    Persistent,
}

pub struct SessionBuilder {
    agent: Agent,
    mode: SessionMode,
}

impl SessionBuilder {
    pub(crate) fn new(agent: Agent) -> Self {
        Self {
            agent,
            mode: SessionMode::Ephemeral,
        }
    }

    pub fn ephemeral(mut self) -> Self {
        self.mode = SessionMode::Ephemeral;
        self
    }

    pub fn persistent(mut self) -> Self {
        self.mode = SessionMode::Persistent;
        self
    }

    pub fn open(self) -> Result<Session, SdkError> {
        Session::open(self.agent, self.mode)
    }
}

/// The tools one session runs with, plus the mutable built-in state they
/// share. Built at open/resume/fork so two sessions never share a process
/// manager, REPL, guard, or todo list unless the agent was configured
/// with a caller-owned instance.
struct SessionTools {
    file_guard: FileGuard,
    todo_list: Option<TodoList>,
    tools: Vec<Arc<dyn Tool>>,
}

impl SessionTools {
    fn new(agent: &Agent) -> Self {
        let definition = &agent.inner;
        let file_guard = definition.shared_file_guard.clone().unwrap_or_default();
        let todo_list = definition
            .wants_todos()
            .then(|| definition.shared_todo_list.clone().unwrap_or_default());
        let tools = session_tools(definition, &file_guard, todo_list.as_ref());
        Self {
            file_guard,
            todo_list,
            tools,
        }
    }

    fn clear(&self) {
        self.file_guard.clear();
        if let Some(todos) = &self.todo_list {
            todos.clear();
        }
    }
}

pub struct Session {
    agent: Agent,
    tools: SessionTools,
    context: Arc<Mutex<Context>>,
    recorder: Option<Arc<SessionHandler>>,
    truncation_store: TruncationStore,
    busy: Arc<AtomicBool>,
    load_warnings: Vec<String>,
}

impl Session {
    fn open(agent: Agent, mode: SessionMode) -> Result<Self, SdkError> {
        let mut context = Context::new();
        if let Some(prompt) = &agent.inner.system_prompt {
            context.push_system(prompt.clone());
        }
        let recorder = match mode {
            SessionMode::Ephemeral => None,
            SessionMode::Persistent => {
                std::fs::create_dir_all(&agent.inner.harness.inner.sessions_dir)?;
                let handler = SessionHandler::create(
                    &agent.inner.harness.inner.sessions_dir,
                    &agent.inner.harness.workspace().root().display().to_string(),
                    &agent.inner.model_name,
                )?;
                handler.sync(&context);
                Some(Arc::new(handler))
            }
        };
        let truncation_store = session_store(&agent);
        Ok(Self {
            tools: SessionTools::new(&agent),
            agent,
            context: Arc::new(Mutex::new(context)),
            recorder,
            truncation_store,
            busy: Arc::new(AtomicBool::new(false)),
            load_warnings: Vec::new(),
        })
    }

    /// Resume a persistent session from disk. The transcript and recovery
    /// store are restored; live processes, REPL state, the read-before-write
    /// guard, and todos start fresh.
    pub(crate) fn resume(agent: Agent, id: &str) -> Result<Self, SdkError> {
        let path = agent
            .inner
            .harness
            .sessions()
            .list()
            .into_iter()
            .find(|file| file.meta.id == id)
            .map(|file| file.path)
            .ok_or_else(|| SdkError::SessionNotFound(id.to_string()))?;
        let (handler, loaded) = SessionHandler::resume(&path)?;
        let truncation_store = load_session_store(&agent, Some(&path))?;
        Ok(Self {
            tools: SessionTools::new(&agent),
            agent,
            context: Arc::new(Mutex::new(loaded.context)),
            recorder: Some(Arc::new(handler)),
            truncation_store,
            busy: Arc::new(AtomicBool::new(false)),
            load_warnings: loaded.warnings,
        })
    }

    pub fn id(&self) -> Option<String> {
        self.recorder.as_ref().map(|handler| handler.session_id())
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.recorder.as_ref().map(|handler| handler.path())
    }

    pub fn load_warnings(&self) -> &[String] {
        &self.load_warnings
    }

    /// The read-before-write guard this session's file tools consult.
    /// Session-owned unless the agent set [`AgentBuilder::file_guard`].
    ///
    /// [`AgentBuilder::file_guard`]: crate::AgentBuilder::file_guard
    pub fn file_guard(&self) -> &FileGuard {
        &self.tools.file_guard
    }

    /// The todo list behind this session's `todo_write` tool, when the
    /// agent enabled todos. Session-owned unless the agent set
    /// [`AgentBuilder::todos_shared`].
    ///
    /// [`AgentBuilder::todos_shared`]: crate::AgentBuilder::todos_shared
    pub fn todo_list(&self) -> Option<TodoList> {
        self.tools.todo_list.clone()
    }

    pub async fn messages(&self) -> Vec<orca_harness_core::Message> {
        self.context.lock().await.messages().to_vec()
    }

    pub async fn run(&self, request: impl Into<RunRequest>) -> Result<RunResult, SdkError> {
        let busy = BusyGuard::acquire(self.busy.clone())?;
        let request = request.into();
        let cancellation = CancellationToken::new();
        execute(RunExecution {
            definition: self.agent.clone(),
            tools: self.tools.tools.clone(),
            context: self.context.clone(),
            recorder: self.recorder.clone(),
            store: self.truncation_store.clone(),
            request,
            cancellation,
            event_send: None,
            _busy: busy,
        })
        .await
    }

    pub fn start(&self, request: impl Into<RunRequest>) -> Result<RunHandle, SdkError> {
        let busy = BusyGuard::acquire(self.busy.clone())?;
        let request = request.into();
        let cancellation = CancellationToken::new();
        let (send, receive) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(execute(RunExecution {
            definition: self.agent.clone(),
            tools: self.tools.tools.clone(),
            context: self.context.clone(),
            recorder: self.recorder.clone(),
            store: self.truncation_store.clone(),
            request,
            cancellation: cancellation.clone(),
            event_send: Some(send),
            _busy: busy,
        }));
        Ok(RunHandle::new(cancellation, receive, task))
    }

    pub async fn compact(&self, config: CompactConfig) -> Result<CompactReport, SdkError> {
        let _busy = BusyGuard::acquire(self.busy.clone())?;
        let mut context = self.context.lock().await;
        let report = compact(&mut context, &self.truncation_store, &config)
            .map_err(|error| SdkError::Config(error.to_string()))?;
        if let Some(recorder) = &self.recorder {
            recorder.sync(&context);
            save_session_store(&self.truncation_store, &recorder.path())?;
        }
        Ok(report)
    }

    /// Copy this persistent session into a new one. The fork shares the
    /// transcript and recovery store but not live processes, REPL state,
    /// the read-before-write guard, or todos: those start fresh unless the
    /// agent configured caller-owned instances.
    pub async fn fork(&self) -> Result<Self, SdkError> {
        let _busy = BusyGuard::acquire(self.busy.clone())?;
        let recorder = self.recorder.as_ref().ok_or(SdkError::EphemeralSession)?;
        let context = self.context.lock().await.clone();
        let original_path = recorder.path();
        let result = (|| -> Result<Self, SdkError> {
            recorder.fork()?;
            let fork_path = recorder.path();
            recorder.sync(&context);
            let fork_store = self.truncation_store.snapshot();
            save_session_store(&fork_store, &fork_path)?;
            let (new_handler, loaded) = SessionHandler::resume(&fork_path)?;
            Ok(Self {
                tools: SessionTools::new(&self.agent),
                agent: self.agent.clone(),
                context: Arc::new(Mutex::new(loaded.context)),
                recorder: Some(Arc::new(new_handler)),
                truncation_store: fork_store,
                busy: Arc::new(AtomicBool::new(false)),
                load_warnings: loaded.warnings,
            })
        })();
        let restored = recorder.switch_to(&original_path);
        match (result, restored) {
            (Ok(session), Ok(_)) => Ok(session),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
        }
    }

    /// Start a new conversation in this session and clear its
    /// read-before-write guard and todo list.
    pub async fn clear(&self) -> Result<(), SdkError> {
        let _busy = BusyGuard::acquire(self.busy.clone())?;
        let mut fresh = Context::new();
        if let Some(prompt) = &self.agent.inner.system_prompt {
            fresh.push_system(prompt.clone());
        }
        let mut context = self.context.lock().await;
        if let Some(recorder) = &self.recorder {
            recorder.start_new_with_context(&fresh)?;
            self.truncation_store.clear();
            save_session_store(&self.truncation_store, &recorder.path())?;
        } else {
            self.truncation_store.clear();
        }
        *context = fresh;
        self.tools.clear();
        Ok(())
    }

    pub async fn reset_in_place(&self) -> Result<(), SdkError> {
        let _busy = BusyGuard::acquire(self.busy.clone())?;
        let recorder = self.recorder.as_ref().ok_or(SdkError::EphemeralSession)?;
        let mut fresh = Context::new();
        if let Some(prompt) = &self.agent.inner.system_prompt {
            fresh.push_system(prompt.clone());
        }
        let mut context = self.context.lock().await;
        recorder.reset()?;
        self.truncation_store.clear();
        *context = fresh;
        recorder.sync(&context);
        save_session_store(&self.truncation_store, &recorder.path())?;
        Ok(())
    }
}

struct BusyGuard {
    busy: Arc<AtomicBool>,
}

impl BusyGuard {
    fn acquire(busy: Arc<AtomicBool>) -> Result<Self, SdkError> {
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| SdkError::BusySession)?;
        Ok(Self { busy })
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
    }
}
