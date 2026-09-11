//! A single run: assembles the core agent from the session's definition
//! and tools, drives it to completion, then persists transcript and
//! recovery store. Reports a [`RunOutcome`] whichever way the run ended,
//! so partial usage and transcript survive failure and cancellation.
//!
//! Detached subagent results reach the parent here and only here: the
//! completion delivery extension drains the session's inbox at each
//! model boundary of a run, ahead of compaction and recording, so the
//! delivered turn is budgeted and persisted like any other. No run is
//! ever started on the session's own initiative.

use std::sync::Arc;

use orca_harness_core::{
    Agent as CoreAgent, CancellationToken, Context, Extension, Limits, Message, Tool,
};
use orca_harness_extensions::{
    ContextCapacity, EventStream, LongSession, ReadToolResultTool, SessionHandler, ToolRetry,
    Truncation, TruncationStore, UsageMeter,
};
use orca_harness_tool_extensions::skills::SkillOnce;
use orca_harness_tools::{ActiveInventory, CompletionDelivery};
use tokio::sync::Mutex;

use super::events::{EventFanout, RunObserver};
use super::persistence::save_session_store;
use super::BusyGuard;
use crate::background::{BackgroundNotification, RunBackground};
use crate::extensions::Compaction;
use crate::{Agent, RunOutcome, RunRequest, SdkError};

pub(super) struct RunExecution {
    pub(super) definition: Agent,
    /// The session's materialized tools (see [`crate::tools::SessionTools`]).
    pub(super) tools: Vec<Arc<dyn Tool>>,
    pub(super) context: Arc<Mutex<Context>>,
    pub(super) recorder: Option<Arc<SessionHandler>>,
    pub(super) store: TruncationStore,
    pub(super) request: RunRequest,
    pub(super) cancellation: CancellationToken,
    /// The observation channel of a background run; `None` for
    /// [`Session::run`](super::Session::run).
    pub(super) observer: Option<RunObserver>,
    /// The session's completion inbox and manager, when the agent
    /// configured subagents.
    pub(super) background: Option<RunBackground>,
    pub(super) _busy: BusyGuard,
}

/// A continuation needs a turn to continue from: anything beyond the
/// system prompt.
fn ensure_continuable(context: &Context) -> Result<(), SdkError> {
    let has_turns = context
        .messages()
        .iter()
        .any(|message| !matches!(message, Message::System { .. }));
    if has_turns {
        Ok(())
    } else {
        Err(SdkError::InvalidContext(
            "nothing to continue: the session has no messages beyond the system prompt".into(),
        ))
    }
}

/// Drive one run. `Err` only when nothing ran: a continuation with nothing
/// to continue. Every other way the run can end is inside the outcome.
pub(super) async fn execute(run: RunExecution) -> Result<RunOutcome, SdkError> {
    let RunExecution {
        definition,
        tools,
        context,
        recorder,
        store,
        request,
        cancellation,
        observer,
        background,
        _busy,
    } = run;
    let mut context = context.lock().await;
    if request.continuation {
        ensure_continuable(&context)?;
    } else {
        context.push_user_with_images(request.prompt, request.images);
    }
    if let Some(background) = &background {
        // Whatever run follows a wake-up request is the one that answers
        // it; a host that continued the session for another reason
        // delivers the batch just the same.
        background.inbox.consume_wakeup();
    }

    let mut limits: Limits = definition.inner.limits.clone();
    if let Some(duration) = request.deadline {
        limits.deadline = Some(tokio::time::Instant::now() + duration);
    }
    let (meter, usage) = UsageMeter::new();
    let fanout = Arc::new(EventFanout::new(request.on_event, observer));
    let events = EventStream::new(fanout.clone());

    let continue_at_step_limit = request.continue_at_step_limit && limits.max_steps > 0;
    let mut agent = CoreAgent::new(definition.inner.model.clone()).limits(limits);
    for tool in tools {
        agent = agent.tool_arc(tool);
    }
    for extension in &definition.inner.extensions {
        agent = agent.extension_arc(extension.clone());
    }
    if definition.inner.extension_config.events {
        agent = agent.extension(events);
    }
    // Ahead of the meter, truncation, compaction, and the recorder: the
    // delivered user turn is metered, budgeted, and persisted like one
    // the host sent.
    if let Some(background) = &background {
        let events = background.events.clone();
        agent = agent.extension(
            CompletionDelivery::new(background.inbox.clone()).on_delivered(move |batch| {
                let _ = events.send(BackgroundNotification::Delivered {
                    spawn_ids: batch.iter().map(|n| n.spawn.id).collect(),
                });
            }),
        );
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
    // After compaction: restores the worker inventory if compaction
    // removed the snapshot delivery refreshed in this same step.
    if let Some(background) = &background {
        agent = agent.extension(ActiveInventory::new(background.manager.clone()));
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
    if let Some(background) = &background {
        // The wake-up latch is set by the first result that arrives after
        // it was consumed; a result that arrived mid-run was delivered by
        // this run, so clear it again or the first result to arrive while
        // idle would never announce itself.
        background.inbox.consume_wakeup();
    }
    let dropped_events = fanout.flush();
    let persistence = match &recorder {
        Some(recorder) => {
            recorder.sync(&context);
            save_session_store(&store, &recorder.path())
        }
        None => Ok(()),
    };
    Ok(RunOutcome {
        execution: run_result.map_err(SdkError::from),
        usage: usage.total(),
        metered_steps: usage.metered_steps(),
        messages: context.messages().to_vec(),
        persistence,
        dropped_events,
    })
}
