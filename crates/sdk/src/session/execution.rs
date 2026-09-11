//! A single run: assembles the core agent from the session's definition
//! and tools, drives it to completion, then persists transcript and
//! recovery store.

use std::sync::Arc;

use orca_harness_core::{Agent as CoreAgent, CancellationToken, Context, Extension, Limits, Tool};
use orca_harness_extensions::{
    ContextCapacity, EventStream, LongSession, ReadToolResultTool, SessionHandler, ToolRetry,
    Truncation, TruncationStore, UsageMeter,
};
use orca_harness_tool_extensions::skills::SkillOnce;
use tokio::sync::Mutex;

use super::persistence::save_session_store;
use super::BusyGuard;
use crate::extensions::Compaction;
use crate::{Agent, HarnessEvent, RunRequest, RunResult, SdkError};

pub(super) struct RunExecution {
    pub(super) definition: Agent,
    /// The session's materialized tools (see [`crate::tools::SessionTools`]).
    pub(super) tools: Vec<Arc<dyn Tool>>,
    pub(super) context: Arc<Mutex<Context>>,
    pub(super) recorder: Option<Arc<SessionHandler>>,
    pub(super) store: TruncationStore,
    pub(super) request: RunRequest,
    pub(super) cancellation: CancellationToken,
    pub(super) event_send: Option<tokio::sync::mpsc::UnboundedSender<HarnessEvent>>,
    pub(super) _busy: BusyGuard,
}

pub(super) async fn execute(run: RunExecution) -> Result<RunResult, SdkError> {
    let RunExecution {
        definition,
        tools,
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
    for tool in tools {
        agent = agent.tool_arc(tool);
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
