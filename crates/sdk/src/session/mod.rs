//! Session lifecycle: open/resume/fork/clear/reset, the session-owned tool
//! state, and the busy guard that serializes operations on one session.

mod builder;
mod events;
mod execution;
mod import;
mod persistence;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::{Context, Message};
use orca_harness_extensions::{
    compact, CompactConfig, CompactReport, SessionHandler, TruncationStore,
};
use orca_harness_tools::{FileGuard, TodoList};
use tokio::sync::{broadcast, Mutex};

use crate::background::{BackgroundNotification, Subagents};
use crate::tools::SessionTools;
use crate::{Agent, RunHandle, RunOutcome, RunRequest, RunResult, SdkError};

use events::RunObserver;
use execution::{execute, RunExecution};
use import::imported_context;
use persistence::{load_session_store, save_session_store, session_store};

pub use builder::{SessionBuilder, SessionMode, Sessions};

pub struct Session {
    agent: Agent,
    tools: SessionTools,
    context: Arc<Mutex<Context>>,
    recorder: Option<Arc<SessionHandler>>,
    truncation_store: TruncationStore,
    busy: Arc<AtomicBool>,
    /// Set by [`shutdown`](Self::shutdown); every later operation is
    /// refused with [`SdkError::SessionClosed`].
    closed: Arc<AtomicBool>,
    load_warnings: Vec<String>,
}

/// An empty conversation carrying only the agent's system prompt, if any.
fn fresh_context(agent: &Agent) -> Context {
    let mut context = Context::new();
    if let Some(prompt) = &agent.inner.system_prompt {
        context.push_system(prompt.clone());
    }
    context
}

impl Session {
    fn open(agent: Agent, mode: SessionMode, imported: Vec<Message>) -> Result<Self, SdkError> {
        let context = if imported.is_empty() {
            fresh_context(&agent)
        } else {
            imported_context(&agent, imported)?
        };
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
            tools: SessionTools::new(&agent.inner),
            agent,
            context: Arc::new(Mutex::new(context)),
            recorder,
            truncation_store,
            busy: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
            load_warnings: Vec::new(),
        })
    }

    /// Resume a persistent session from disk. The transcript and recovery
    /// store are restored; live processes, REPL state, the read-before-write
    /// guard, todos, and detached subagents start fresh.
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
            tools: SessionTools::new(&agent.inner),
            agent,
            context: Arc::new(Mutex::new(loaded.context)),
            recorder: Some(Arc::new(handler)),
            truncation_store,
            busy: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
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

    /// The read-before-write guard this session's file tools consult (a
    /// shared handle). Session-owned unless the agent set
    /// [`AgentBuilder::file_guard`].
    ///
    /// [`AgentBuilder::file_guard`]: crate::AgentBuilder::file_guard
    pub fn file_guard(&self) -> FileGuard {
        self.tools.file_guard.clone()
    }

    /// The todo list behind this session's `todo_write` tool, when the
    /// agent enabled todos. Session-owned unless the agent set
    /// [`AgentBuilder::todos_shared`].
    ///
    /// [`AgentBuilder::todos_shared`]: crate::AgentBuilder::todos_shared
    pub fn todo_list(&self) -> Option<TodoList> {
        self.tools.todo_list.clone()
    }

    /// Typed host access to this session's subagents, when the agent
    /// configured them with [`AgentBuilder::subagents`]. The handle shares
    /// the session's manager and completion inbox with the `subagent`
    /// model tool; a fork or resume starts with no live jobs.
    ///
    /// [`AgentBuilder::subagents`]: crate::AgentBuilder::subagents
    pub fn subagents(&self) -> Option<Subagents> {
        self.tools
            .background
            .as_ref()
            .map(|services| services.handle(self.closed.clone()))
    }

    /// Observe this session's detached subagents: worker exits, results
    /// waiting for the parent, and batches entering the transcript.
    /// `None` when the agent configured no subagents.
    ///
    /// Observation is separate from delivery. Reading a notification
    /// consumes nothing the parent is owed, and the session never starts
    /// a run on its own: a host that wants the parent to take up results
    /// that arrived while idle awaits
    /// [`BackgroundNotification::CompletionsReady`] and calls
    /// [`continue_run`](Self::continue_run) (or [`run`](Self::run) with
    /// the next prompt) itself; the batch is delivered at that run's
    /// first model call. A receiver that falls more than
    /// [`NOTIFICATION_CAPACITY`](crate::NOTIFICATION_CAPACITY)
    /// notifications behind misses the oldest ones.
    pub fn notifications(&self) -> Option<broadcast::Receiver<BackgroundNotification>> {
        self.tools
            .background
            .as_ref()
            .map(|services| services.subscribe())
    }

    /// Detached results waiting for the parent transcript; zero without
    /// configured subagents.
    pub fn pending_completions(&self) -> usize {
        self.tools
            .background
            .as_ref()
            .map_or(0, |services| services.pending_completions())
    }

    pub async fn messages(&self) -> Vec<orca_harness_core::Message> {
        self.context.lock().await.messages().to_vec()
    }

    /// Run one request to completion on this session. A request built with
    /// [`RunRequest::continuation`] behaves as [`continue_run`](Self::continue_run).
    /// The convenience view of [`run_outcome`](Self::run_outcome): a run
    /// or persistence failure is the `Err`, and the partial accounting
    /// is dropped with it.
    pub async fn run(&self, request: impl Into<RunRequest>) -> Result<RunResult, SdkError> {
        self.run_outcome(request).await?.into_result()
    }

    /// Run one request to completion and report its detailed
    /// [`RunOutcome`], including usage and transcript when the run failed
    /// or was cancelled. `Err` only when the run never started: the
    /// session is busy, or the request shape cannot run (a continuation
    /// with a prompt, images, or nothing to continue).
    pub async fn run_outcome(
        &self,
        request: impl Into<RunRequest>,
    ) -> Result<RunOutcome, SdkError> {
        let busy = self.acquire()?;
        execute(self.prepare_run(request.into(), None, busy)?).await
    }

    /// Continue the conversation from where it stands without appending a
    /// user message: the model produces the next assistant turn on the
    /// current transcript. Limits, the request deadline, and the busy guard
    /// apply as for [`run`](Self::run). Fails with
    /// [`SdkError::InvalidContext`] when the transcript holds nothing beyond
    /// the system prompt, and with [`SdkError::Config`] when the request
    /// carries a prompt or images.
    pub async fn continue_run(&self, request: RunRequest) -> Result<RunResult, SdkError> {
        self.run(RunRequest {
            continuation: true,
            ..request
        })
        .await
    }

    /// Start a request in the background. The handle's token is the run's
    /// own: cancelling it, or dropping the handle, cancels this run and
    /// nothing else, in particular not a caller token supplied through
    /// [`RunRequest::cancellation`].
    ///
    /// A continuation on a transcript with nothing beyond the system prompt
    /// is not rejected here: the run fails with [`SdkError::InvalidContext`]
    /// from [`RunHandle::finish`] or [`RunHandle::outcome`] before any
    /// model call.
    ///
    /// The handle's event stream is bounded by
    /// [`RunRequest::event_capacity`] and never blocks the run.
    pub fn start(&self, request: impl Into<RunRequest>) -> Result<RunHandle, SdkError> {
        let busy = self.acquire()?;
        let request = request.into();
        let (observer, receiver, dropped) = RunObserver::channel(request.event_capacity);
        let run = self.prepare_run(request, Some(observer), busy)?;
        let cancellation = run.cancellation.clone();
        let task = tokio::spawn(execute(run));
        Ok(RunHandle::new(cancellation, receiver, dropped, task))
    }

    /// The single construction site for a run over this session's state.
    /// Rejects request shapes that cannot run, and derives the run's token
    /// from the caller's when one was supplied.
    fn prepare_run(
        &self,
        request: RunRequest,
        observer: Option<RunObserver>,
        busy: BusyGuard,
    ) -> Result<RunExecution, SdkError> {
        if request.continuation && !request.prompt.is_empty() {
            return Err(SdkError::Config(
                "a continuation carries no prompt; use RunRequest::new for a new turn".into(),
            ));
        }
        if request.continuation && !request.images.is_empty() {
            return Err(SdkError::Config(
                "a continuation carries no user message to attach images to".into(),
            ));
        }
        let cancellation = request.run_token();
        Ok(RunExecution {
            definition: self.agent.clone(),
            tools: self.tools.tools.clone(),
            context: self.context.clone(),
            recorder: self.recorder.clone(),
            store: self.truncation_store.clone(),
            request,
            cancellation,
            observer,
            background: self
                .tools
                .background
                .as_ref()
                .map(|services| services.run_handles()),
            _busy: busy,
        })
    }

    pub async fn compact(&self, config: CompactConfig) -> Result<CompactReport, SdkError> {
        let _busy = self.acquire()?;
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
    /// the read-before-write guard, todos, or detached subagents: those
    /// start fresh unless the agent configured caller-owned instances. In
    /// particular the fork has its own subagent manager, completion inbox,
    /// and notification channel; results owed to this session never reach
    /// the fork.
    pub async fn fork(&self) -> Result<Self, SdkError> {
        let _busy = self.acquire()?;
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
                tools: SessionTools::new(&self.agent.inner),
                agent: self.agent.clone(),
                context: Arc::new(Mutex::new(loaded.context)),
                recorder: Some(Arc::new(new_handler)),
                truncation_store: fork_store,
                busy: Arc::new(AtomicBool::new(false)),
                closed: Arc::new(AtomicBool::new(false)),
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
    /// read-before-write guard and todo list, cancelling any detached
    /// subagents and dropping their undelivered results. The subagent
    /// manager starts a new generation, so a worker that finishes after
    /// this call has its result refused rather than delivered to the new
    /// conversation.
    pub async fn clear(&self) -> Result<(), SdkError> {
        let _busy = self.acquire()?;
        let fresh = fresh_context(&self.agent);
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

    /// Empty this persistent session's transcript in place, keeping its
    /// id and file. Like [`clear`](Self::clear), the guard, todos, and
    /// detached subagents are reset with it.
    pub async fn reset_in_place(&self) -> Result<(), SdkError> {
        let _busy = self.acquire()?;
        let recorder = self.recorder.as_ref().ok_or(SdkError::EphemeralSession)?;
        let fresh = fresh_context(&self.agent);
        let mut context = self.context.lock().await;
        recorder.reset()?;
        self.truncation_store.clear();
        *context = fresh;
        recorder.sync(&context);
        save_session_store(&self.truncation_store, &recorder.path())?;
        self.tools.clear();
        Ok(())
    }

    /// Stop this session for good: refuse every later run, spawn, and
    /// conversation change, cancel its detached workers, and wait, up to
    /// `grace`, for them to exit. Admission stops before the wait begins,
    /// so nothing can extend it; undelivered results are dropped.
    /// Cancellation is cooperative: a worker that ignores its token keeps
    /// the wait going, and when `grace` runs out the error reports how
    /// many are still winding down. A worker's exit notification may
    /// still be in flight when this returns.
    ///
    /// Fails with [`SdkError::BusySession`] while a run is active,
    /// touching nothing; finish or cancel the run and call again. Every
    /// operation after a shutdown, this one included, fails with
    /// [`SdkError::SessionClosed`]; [`subagents`](Self::subagents) and
    /// [`notifications`](Self::notifications) still observe what is
    /// winding down. A session without configured subagents closes at
    /// once. Unlike [`clear`](Self::clear), which starts a fresh
    /// conversation the session keeps serving, this is final. A host
    /// foreground worker ([`Subagents::run`](crate::Subagents::run)) in
    /// flight is neither cancelled nor awaited: foreground workers bypass
    /// admission and follow the caller's token, so cancel it through
    /// that token.
    ///
    /// Dropping a session instead of calling this cancels the same work
    /// but waits for nothing. Background processes and workflow runs are
    /// not yet covered here.
    pub async fn shutdown(&self, grace: Duration) -> Result<(), SdkError> {
        let _busy = self.acquire()?;
        self.closed.store(true, Ordering::Release);
        let Some(services) = &self.tools.background else {
            return Ok(());
        };
        services.close();
        let manager = services.manager();
        match tokio::time::timeout(grace, manager.wait_idle()).await {
            Ok(()) => Ok(()),
            Err(_) => Err(SdkError::ShutdownTimeout {
                still_active: manager.live_workers(),
            }),
        }
    }

    /// Serialize operations on this session: a closed session refuses
    /// them, a busy one rejects overlap.
    fn acquire(&self) -> Result<BusyGuard, SdkError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SdkError::SessionClosed);
        }
        BusyGuard::acquire(self.busy.clone())
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
