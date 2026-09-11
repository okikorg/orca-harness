//! Typed host operations over the same implementation the `subagent`
//! model tool runs.
//!
//! A host (the SDK, an embedding application) spawns and controls
//! workers without composing tool-call JSON or a `ToolContext`. Every
//! entry point here and [`Tool::call`] funnel through one private path:
//! [`SubagentRequest`] -> `SpawnRequest` -> `prepare_spawn` -> detach or
//! run. Host-originated spawns are therefore subject to the same routing
//! validation, admission limits, and spawn extensions as model-originated
//! ones; the only observable difference is the synthetic `host:<spawn id>`
//! call id in [`SubagentSpawn::call_id`].

use super::spawn::{Prepared, SpawnRequest};
use super::*;

/// One task for a spawned agent, as a host names it.
#[derive(Clone, Debug)]
pub struct SubagentRequest {
    pub(crate) task: String,
    pub(crate) system_prompt: Option<String>,
    pub(crate) model: Option<String>,
}

impl SubagentRequest {
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            system_prompt: None,
            model: None,
        }
    }

    /// Override the tool's default system prompt for this worker.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Request one host-approved model by id. Validated against the live
    /// `/subagents` route exactly like a model-supplied `model` argument.
    pub fn model(mut self, id: impl Into<String>) -> Self {
        self.model = Some(id.into());
        self
    }

    pub fn task(&self) -> &str {
        &self.task
    }

    pub fn system_prompt_override(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    pub fn requested_model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

/// A finished foreground worker: the typed form of the tool's JSON result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentOutcome {
    pub answer: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub runtime_ms: u128,
    pub steps: u64,
    pub tool_calls: u64,
    pub identity: Option<SubagentIdentity>,
}

impl SubagentOutcome {
    /// The JSON the `subagent` tool returns for a completed foreground run.
    pub fn into_value(self) -> Value {
        json!({
            "answer": self.answer,
            "usage": {
                "inputTokens": self.input_tokens,
                "outputTokens": self.output_tokens,
            },
            "runtimeMs": self.runtime_ms,
            "steps": self.steps,
            "toolCalls": self.tool_calls,
            "termination": "completed",
            "identity": self.identity,
        })
    }
}

/// An admitted detached worker: the typed form of the tool's acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundAcknowledgement {
    pub spawn_id: u64,
    pub status: BackgroundStatus,
    pub identity: Option<SubagentIdentity>,
}

impl BackgroundAcknowledgement {
    /// The JSON the `subagent` tool returns for a `background: true` call.
    pub fn into_value(self) -> Value {
        json!({
            "spawnId": self.spawn_id,
            "status": self.status.as_str(),
            "termination": "detached",
            "identity": self.identity,
        })
    }
}

impl<M: Model + Clone + 'static> SubagentTool<M> {
    /// Whether detached (`background`) execution is configured here.
    pub fn has_background(&self) -> bool {
        self.background.is_some()
    }

    /// Run one worker to completion in the foreground.
    ///
    /// Host-originated spawns are subject to the same routing validation,
    /// admission limits, and spawn extensions as model-originated ones.
    /// `cancellation` is honoured as-is (the tool path derives a child of
    /// the calling turn's token); `deadline` bounds the run together with
    /// the configured limits and the live worker timeout.
    pub async fn run_foreground(
        &self,
        request: SubagentRequest,
        cancellation: CancellationToken,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<SubagentOutcome, ToolError> {
        let prepared = self.prepare_request(request, None, false, deadline)?;
        Self::execute_foreground(prepared, cancellation).await
    }

    /// Admit one detached worker and return its acknowledgement at once.
    /// The result reaches the notifier given to [`Self::background`].
    ///
    /// Host-originated spawns are subject to the same routing validation,
    /// admission limits, and spawn extensions as model-originated ones.
    /// Fails, with the tool's message, when background execution is not
    /// configured at this depth.
    pub fn spawn_background(
        &self,
        request: SubagentRequest,
    ) -> Result<BackgroundAcknowledgement, ToolError> {
        Ok(self.prepare_request(request, None, true, None)?.detach())
    }

    /// Admitted detached workers, running and queued, in spawn order.
    /// Empty when background execution is not configured.
    ///
    /// Unlike the tool's `action=list`, this never refuses a repeat with
    /// "no change since the previous list": that refusal keeps the model
    /// from polling, whereas a host may poll this freely.
    pub fn active_jobs(&self) -> Vec<BackgroundJob> {
        self.background
            .as_ref()
            .map(|config| config.manager.active())
            .unwrap_or_default()
    }

    /// Cancel one detached worker; `false` when it is unknown or already
    /// finished, or when background execution is not configured.
    pub fn cancel_job(&self, spawn_id: u64) -> bool {
        self.background
            .as_ref()
            .is_some_and(|config| config.manager.cancel(spawn_id))
    }

    /// Cancel every detached worker and report how many were admitted.
    pub fn cancel_all_jobs(&self) -> usize {
        self.background
            .as_ref()
            .map_or(0, |config| config.manager.cancel_all())
    }

    /// The single request path shared by the model tool and the typed
    /// API. `call_id` is `None` for host-originated spawns, which get the
    /// synthetic `host:<spawn id>` id so spawn extensions can tell them
    /// from model tool calls.
    pub(super) fn prepare_request(
        &self,
        request: SubagentRequest,
        call_id: Option<String>,
        detached: bool,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Prepared<M>, ToolError> {
        if detached && self.background.is_none() {
            return Err(ToolError::msg(
                "background subagents are unavailable in this host or nesting depth",
            ));
        }
        let id = self.next_spawn_id();
        let req = SpawnRequest {
            id,
            generation: None,
            expected_model_key: None,
            depth: None,
            task: request.task,
            system_prompt: request.system_prompt,
            model: request.model,
            call_id: call_id.unwrap_or_else(|| format!("host:{id}")),
            run: None,
            stage: None,
            parent_id: None,
            notifier: None,
        };
        self.prepare_spawn(req, detached, if detached { None } else { deadline })
    }

    /// Drive a prepared foreground worker and account for it however it ends.
    pub(super) async fn execute_foreground(
        prepared: Prepared<M>,
        cancellation: CancellationToken,
    ) -> Result<SubagentOutcome, ToolError> {
        let Prepared {
            agent,
            spawn,
            telemetry,
            started,
            in_flight,
            ..
        } = prepared;
        let _in_flight = in_flight;
        let result = agent.run_with_cancellation(&spawn.task, cancellation).await;
        subagent_outcome(result, &telemetry, started, spawn.identity.as_ref())
            .map_err(ToolError::msg)
    }
}
