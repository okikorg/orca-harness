use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use orca_harness_core::{Agent as CoreAgent, CancellationToken, Context, Extension, Limits};
use orca_harness_extensions::{
    compact, CompactConfig, CompactReport, ContextCapacity, EventStream, LongSession,
    ReadToolResultTool, SessionFile, SessionHandler, ToolRetry, Truncation, TruncationStore,
    UsageMeter,
};
use orca_harness_tool_extensions::skills::SkillOnce;
use tokio::sync::Mutex;

use crate::extensions::Compaction;
use crate::{Agent, HarnessEvent, RunHandle, RunRequest, RunResult, SdkError};

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

pub struct Session {
    agent: Agent,
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
            agent,
            context: Arc::new(Mutex::new(context)),
            recorder,
            truncation_store,
            busy: Arc::new(AtomicBool::new(false)),
            load_warnings: Vec::new(),
        })
    }

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

    pub async fn messages(&self) -> Vec<orca_harness_core::Message> {
        self.context.lock().await.messages().to_vec()
    }

    pub async fn run(&self, request: impl Into<RunRequest>) -> Result<RunResult, SdkError> {
        let busy = BusyGuard::acquire(self.busy.clone())?;
        let request = request.into();
        let cancellation = CancellationToken::new();
        execute(RunExecution {
            definition: self.agent.clone(),
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
        self.agent.inner.file_guard.clear();
        if let Some(todos) = &self.agent.inner.todo_list {
            todos.clear();
        }
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

fn session_store(agent: &Agent) -> TruncationStore {
    TruncationStore::new(session_store_budget(agent))
}

fn load_session_store(
    agent: &Agent,
    session_path: Option<&Path>,
) -> Result<TruncationStore, SdkError> {
    match session_path {
        Some(path) => Ok(TruncationStore::load(
            &recovery_path(path),
            session_store_budget(agent),
        )?),
        None => Ok(session_store(agent)),
    }
}

fn session_store_budget(agent: &Agent) -> usize {
    agent
        .inner
        .extension_config
        .truncation
        .map(|config| config.store_budget_bytes)
        .unwrap_or(16 * 1024 * 1024)
}

fn recovery_path(session_path: &Path) -> PathBuf {
    session_path.with_extension("recovery.json")
}

fn save_session_store(store: &TruncationStore, session_path: &Path) -> Result<(), SdkError> {
    store.save(&recovery_path(session_path))?;
    Ok(())
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

struct RunExecution {
    definition: Agent,
    context: Arc<Mutex<Context>>,
    recorder: Option<Arc<SessionHandler>>,
    store: TruncationStore,
    request: RunRequest,
    cancellation: CancellationToken,
    event_send: Option<tokio::sync::mpsc::UnboundedSender<HarnessEvent>>,
    _busy: BusyGuard,
}

async fn execute(run: RunExecution) -> Result<RunResult, SdkError> {
    let RunExecution {
        definition,
        context,
        recorder,
        store,
        request,
        cancellation,
        event_send,
        _busy,
    } = run;
    let mut context = context.lock().await;
    context.push_user_with_images(request.prompt, request.images);

    let mut limits: Limits = definition.inner.limits.clone();
    if let Some(duration) = request.deadline {
        limits.deadline = Some(tokio::time::Instant::now() + duration);
    }
    let (meter, usage) = UsageMeter::new();
    let callback = request.on_event;
    let events = EventStream::from_fn(move |event| {
        if let Some(callback) = &callback {
            callback(event.clone());
        }
        if let Some(send) = &event_send {
            let _ = send.send(event);
        }
    });

    let continue_at_step_limit = request.continue_at_step_limit && limits.max_steps > 0;
    let mut agent = CoreAgent::new(definition.inner.model.clone()).limits(limits);
    for tool in &definition.inner.tools {
        agent = agent.tool_arc(tool.clone());
    }
    for extension in &definition.inner.extensions {
        agent = agent.extension_arc(extension.clone());
    }
    if definition.inner.extension_config.events {
        agent = agent.extension(events);
    }
    if definition.inner.extension_config.usage {
        agent = agent.extension(meter);
    }
    if let Some(config) = definition.inner.extension_config.truncation {
        agent = agent.extension(Truncation::new(config.max_string_chars).store(store.clone()));
        if config.expose_reader_tool {
            agent = agent.tool(ReadToolResultTool::new(store.clone()));
        }
    }
    if let Some(config) = definition.inner.extension_config.retry {
        agent = agent.extension(ToolRetry::new(config.attempts).backoff(config.duration()));
    }
    if let Compaction::Automatic(config) = definition.inner.extension_config.compaction {
        let capacity = ContextCapacity::new(definition.inner.context_capacity);
        let mut long_session = LongSession::new(capacity, store.clone()).config(config);
        if let Some(callback) = &definition.inner.extension_config.on_compact {
            let callback = callback.clone();
            long_session = long_session.on_compact(move |report| callback(report));
        }
        agent = agent.extension(long_session);
    }
    // After compaction: it reads "already loaded" off the context the
    // model is about to see.
    if definition.inner.skill_once {
        agent = agent.extension(SkillOnce::new());
    }
    if let Some(recorder) = &recorder {
        agent = agent.extension_arc(recorder.clone() as Arc<dyn Extension>);
    }

    let run_result = loop {
        match agent.run_context(&mut context, cancellation.clone()).await {
            Err(orca_harness_core::HarnessError::StepLimitExceeded) if continue_at_step_limit => {
                // A fully synchronous model/tool slice must not starve Stop.
                tokio::task::yield_now().await;
            }
            result => break result,
        }
    };
    let persistence_result = match &recorder {
        Some(recorder) => {
            recorder.sync(&context);
            save_session_store(&store, &recorder.path())
        }
        None => Ok(()),
    };
    let text = match run_result {
        Ok(text) => {
            persistence_result?;
            text
        }
        Err(error) => return Err(error.into()),
    };
    Ok(RunResult {
        text,
        usage: usage.total(),
        metered_steps: usage.metered_steps(),
        messages: context.messages().to_vec(),
    })
}
