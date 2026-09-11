//! Conversation and shutdown lifecycle of a [`Session`]: compaction,
//! forking, clear and reset, and the final bounded shutdown.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orca_harness_extensions::{compact, CompactConfig, CompactReport, SessionHandler};
use orca_harness_tools::ProcessController;
use tokio::sync::Mutex;

use super::persistence::save_session_store;
use super::{fresh_context, Session};
use crate::tools::SessionTools;
use crate::SdkError;

impl Session {
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
    /// the read-before-write guard, todos, detached subagents, or workflow
    /// runs: those start fresh unless the agent configured caller-owned
    /// instances. In particular the fork has its own subagent manager,
    /// completion inbox, workflow store, and notification channel; results
    /// owed to this session never reach the fork, and a run this session
    /// admitted is unknown to the fork's [`Workflows`](crate::Workflows).
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
    /// subagents and workflow runs (with their stage workers), dropping
    /// their undelivered results and stored stage outputs, and killing the
    /// session's background processes (host- and model-started alike,
    /// without exit notifications; the process handle keeps serving).
    /// The subagent manager starts a new generation, so a worker or run
    /// that finishes after this call has its result refused rather than
    /// delivered to the new conversation, and a cancelled run's id is no
    /// longer known to [`Workflows`](crate::Workflows).
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
        // Kills can wait up to 5 s per process: never with the transcript
        // locked.
        drop(context);
        self.tools.clear().await;
        Ok(())
    }

    /// Empty this persistent session's transcript in place, keeping its
    /// id and file. Like [`clear`](Self::clear), the guard, todos,
    /// detached subagents, workflow runs with their stored stage outputs,
    /// and background processes are reset with it.
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
        drop(context);
        self.tools.clear().await;
        Ok(())
    }

    /// Stop this session for good: refuse every later run, spawn,
    /// workflow submission, and conversation change, cancel its detached
    /// workers and workflow runs, kill its background processes, and wait,
    /// up to `grace`, for all of them to exit. Admission stops before the
    /// wait begins, so nothing can extend it; undelivered results are
    /// dropped. Worker cancellation is cooperative: one that ignores its
    /// token keeps the wait going, and when `grace` runs out the error
    /// reports how many workers and processes are still winding down. A
    /// worker's or process's exit notification may still be in flight
    /// when this returns.
    ///
    /// Fails with [`SdkError::BusySession`] while a run is active,
    /// touching nothing; finish or cancel the run and call again. Every
    /// operation after a shutdown, this one included, fails with
    /// [`SdkError::SessionClosed`]; [`subagents`](Self::subagents) and
    /// [`notifications`](Self::notifications) still observe what is
    /// winding down. A session without subagents or running processes
    /// closes at once. Unlike [`clear`](Self::clear), which starts a fresh
    /// conversation the session keeps serving, this is final. A host
    /// foreground worker ([`Subagents::run`](crate::Subagents::run)) in
    /// flight is neither cancelled nor awaited: foreground workers bypass
    /// admission and follow the caller's token, so cancel it through
    /// that token.
    ///
    /// Dropping a session instead of calling this cancels the same work
    /// but waits for nothing. [`processes`](Self::processes) refuses
    /// every operation after a shutdown and reports closed. Workflow runs
    /// are jobs of the same manager: closing it settles each live run as
    /// cancelled (its run-level outcome is still observable on
    /// [`notifications`](Self::notifications), though owed to no one),
    /// the wait covers their stage workers, and
    /// [`workflows`](Self::workflows) afterwards refuses submissions and
    /// knows no runs.
    pub async fn shutdown(&self, grace: Duration) -> Result<(), SdkError> {
        let _busy = self.acquire()?;
        self.closed.store(true, Ordering::Release);
        let services = self.tools.background.as_ref();
        let process = self.tools.process.as_ref();
        if let Some(services) = services {
            services.close();
        }
        if let Some(process) = process {
            process.close();
        }
        let idle = async {
            tokio::join!(
                async {
                    if let Some(services) = services {
                        services.manager().wait_idle().await;
                    }
                },
                async {
                    if let Some(process) = process {
                        process.wait_idle().await;
                    }
                }
            );
        };
        match tokio::time::timeout(grace, idle).await {
            Ok(()) => Ok(()),
            Err(_) => Err(SdkError::ShutdownTimeout {
                still_active_workers: services
                    .map_or(0, |services| services.manager().live_workers()),
                still_running_processes: process.map_or(0, ProcessController::running),
            }),
        }
    }
}
