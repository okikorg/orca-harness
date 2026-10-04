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
            self.environment.persist(&fork_path)?;
            recorder.sync(&context);
            let fork_store = self.truncation_store.snapshot();
            save_session_store(&fork_store, &fork_path)?;
            let (new_handler, loaded) = SessionHandler::resume(&fork_path)?;
            Ok(Self {
                tools: SessionTools::new(&self.agent.inner, &self.environment),
                environment: self.environment.clone(),
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
            let candidate = SessionHandler::create(
                &self.agent.inner.harness.inner.sessions_dir,
                &self
                    .agent
                    .inner
                    .harness
                    .workspace()
                    .root()
                    .display()
                    .to_string(),
                &self.agent.inner.model_name,
            )?;
            adopt_prepared(recorder, candidate, |candidate| {
                self.environment.persist(&candidate.path())?;
                candidate.sync(&fresh);
                verify_candidate(candidate, &fresh)?;
                save_session_store(
                    &super::persistence::session_store(&self.agent),
                    &candidate.path(),
                )
            })?;
            self.truncation_store.clear();
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
    /// SDK-owned Python/Bun interpreters also receive kill requests within
    /// `grace`. Remote interpreter kills use Spawner's best-effort semantics:
    /// errors are not propagated and exit is not confirmed. Timeout counters
    /// describe background workers/process tools and exclude REPL cleanup.
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
                },
                self.tools.reset_repls()
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

/// `sync` logs write failures; verify the persisted candidate explicitly
/// before adopting it so a failed append cannot silently lose the prompt.
fn verify_candidate(
    candidate: &SessionHandler,
    expected: &orca_harness_core::Context,
) -> Result<(), SdkError> {
    let loaded = orca_harness_extensions::SessionFile::load(&candidate.path())?;
    let serialized = |context: &orca_harness_core::Context| {
        serde_json::to_value(context.messages())
            .map_err(|e| SdkError::Config(format!("cannot verify candidate transcript: {e}")))
    };
    if !loaded.warnings.is_empty() || serialized(&loaded.context)? != serialized(expected)? {
        return Err(SdkError::Config(
            "candidate transcript was not completely persisted".into(),
        ));
    }
    Ok(())
}

/// Prepare every companion before switching the active recorder. Failed
/// preparation leaves the original recorder/context untouched.
fn adopt_prepared(
    recorder: &SessionHandler,
    candidate: SessionHandler,
    prepare: impl FnOnce(&SessionHandler) -> Result<(), SdkError>,
) -> Result<(), SdkError> {
    let path = candidate.path();
    let result = prepare(&candidate).and_then(|()| {
        // Reading back also validates candidate transcript before adoption.
        recorder.switch_to(&path)?;
        Ok(())
    });
    if result.is_err() {
        drop(candidate);
        for file in [
            path.clone(),
            path.with_extension("environment.json"),
            path.with_extension("recovery.json"),
        ] {
            let _ = std::fs::remove_file(file);
        }
    }
    result
}

#[cfg(test)]
mod environment_rotation_tests {
    use super::*;

    #[test]
    fn failed_binding_preparation_preserves_original_recorder() {
        let dir = std::env::temp_dir().join(format!(
            "orca-rotation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let recorder = SessionHandler::create(&dir, "/workspace", "test").unwrap();
        let original = recorder.path();
        let environment = crate::SessionEnvironment::Disabled;
        environment.persist(&original).unwrap();
        let candidate = SessionHandler::create(&dir, "/workspace", "test").unwrap();
        let candidate_path = candidate.path();
        let result = adopt_prepared(&recorder, candidate, |candidate| {
            // A directory at the exact sidecar destination deterministically
            // makes the real binding write fail, even when run as root.
            std::fs::create_dir(candidate.path().with_extension("environment.json"))?;
            environment.persist(&candidate.path())
        });
        assert!(result.is_err());
        assert_eq!(recorder.path(), original);
        environment.verify(&original).unwrap();
        assert!(!candidate_path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn incomplete_candidate_cannot_replace_original_transcript() {
        let dir = std::env::temp_dir().join(format!(
            "orca-rotation-content-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let recorder = SessionHandler::create(&dir, "/workspace", "test").unwrap();
        let original = recorder.path();
        let mut context = orca_harness_core::Context::new();
        context.push_system("must survive");
        recorder.sync(&context);
        let candidate = SessionHandler::create(&dir, "/workspace", "test").unwrap();
        let candidate_path = candidate.path();
        let result = adopt_prepared(&recorder, candidate, |candidate| {
            // A valid header with the expected prompt absent models a failed
            // append that sync would log instead of returning to its caller.
            verify_candidate(candidate, &context)
        });
        assert!(result.is_err());
        assert_eq!(recorder.path(), original);
        verify_candidate(&recorder, &context).unwrap();
        assert!(!candidate_path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
