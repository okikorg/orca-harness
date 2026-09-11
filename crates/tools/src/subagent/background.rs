//! Detached worker lifetime, tool protocol, and execution.
use super::{BackgroundStats, Meter, SubagentIdentity, SubagentSpawn};
use orca_harness_core::{Agent, Limits, Model};
use serde_json::{json, Value};
use std::sync::Arc;

mod completions;
mod inventory;
pub(crate) mod manager;
mod protocol;
pub use completions::{
    subagent_completions_prompt, CompletionDelivery, CompletionInbox, DEFAULT_COMPLETION_CAPACITY,
};
pub use inventory::{refresh_inventory, ActiveInventory};
pub use manager::{
    BackgroundJob, BackgroundStatus, SubagentManager, SubagentNotification,
    DEFAULT_BACKGROUND_SUBAGENT_LIMIT,
};
pub(super) use protocol::{subagent_control, subagent_parameters, BACKGROUND_DELIVERY};
pub(crate) type SubagentNotifier = Arc<dyn Fn(SubagentNotification) + Send + Sync>;

/// What the last non-empty `action=list` reported, so a repeat with no
/// change can be refused instead of serving as a polling primitive.
pub(super) type ListSnapshot = Arc<std::sync::Mutex<Option<Vec<(u64, BackgroundStatus)>>>>;

#[derive(Clone)]
pub(crate) struct BackgroundConfig {
    pub(crate) manager: SubagentManager,
    pub(crate) notifier: SubagentNotifier,
    pub(super) last_list: ListSnapshot,
}

impl BackgroundConfig {
    pub(super) fn admit(&self, spawn: &SubagentSpawn) -> Result<manager::Admission, &'static str> {
        self.manager.inner.admit(spawn)
    }
}

/// Decrements the in-flight agent count however the call ends.
pub(super) struct InFlight(pub(super) BackgroundStats);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.dec_agents();
    }
}

pub(super) fn subagent_result(
    result: Result<String, orca_harness_core::HarnessError>,
    telemetry: &Meter,
    started: std::time::Instant,
    identity: Option<&SubagentIdentity>,
) -> Result<Value, String> {
    let elapsed_ms = started.elapsed().as_millis();
    let total = telemetry.total();
    let steps = telemetry.steps();
    let tool_calls = telemetry.tool_calls();
    let answer = result.map_err(|err| {
        format!(
            "subagent failed: {err} (runtimeMs={elapsed_ms}, steps={steps}, toolCalls={tool_calls}, inputTokens={}, outputTokens={})",
            total.input_tokens, total.output_tokens
        )
    })?;
    Ok(json!({
        "answer": answer,
        "usage": {
            "inputTokens": total.input_tokens,
            "outputTokens": total.output_tokens,
        },
        "runtimeMs": elapsed_ms,
        "steps": steps,
        "toolCalls": tool_calls,
        "termination": "completed",
        "identity": identity,
    }))
}

pub(super) fn detach_subagent<M: Model + Clone + 'static>(
    background: (BackgroundConfig, manager::Admission),
    agent: Agent<M>,
    spawn: SubagentSpawn,
    telemetry: Meter,
    mut limits: Limits,
    timeout_secs: u32,
    in_flight: InFlight,
) -> Value {
    let (background, admission) = background;
    let (generation, cancellation, slot) = admission.into_parts();
    let manager = background.manager.inner.clone();
    let status = if slot.is_some() {
        BackgroundStatus::Running
    } else {
        BackgroundStatus::Queued
    };
    let acknowledgement = json!({
        "spawnId": spawn.id,
        "status": status.as_str(),
        "termination": "detached",
        "identity": spawn.identity,
    });
    let job = Completion {
        manager,
        generation,
        spawn: Some(spawn),
        notifier: background.notifier,
        result: Err("subagent execution interrupted".into()),
    };
    tokio::spawn(async move {
        let _in_flight = in_flight;
        let mut job = job;
        let spawn = job.spawn.as_ref().unwrap();
        let slot = match slot {
            Some(slot) => Some(slot),
            None => match limits.deadline.filter(|_| spawn.run.is_some()) {
                Some(deadline) => {
                    tokio::time::timeout_at(deadline, job.manager.acquire(spawn.id, &cancellation))
                        .await
                        .ok()
                        .flatten()
                }
                None => job.manager.acquire(spawn.id, &cancellation).await,
            },
        };
        job.result = if let Some(_slot) = slot {
            // Queue time does not consume the worker's execution timeout.
            let started = std::time::Instant::now();
            limits.deadline = execution_deadline(limits.deadline, timeout_secs);
            let result = agent
                .limits(limits)
                .run_with_cancellation(&spawn.task, cancellation)
                .await;
            subagent_result(result, &telemetry, started, spawn.identity.as_ref())
        } else {
            Err(if cancellation.is_cancelled() {
                "subagent cancelled before execution"
            } else {
                "subagent queue deadline exceeded"
            }
            .into())
        };
    });
    acknowledgement
}

/// Cleanup must also run if a model panics or the runtime drops the task.
struct Completion {
    manager: Arc<manager::SubagentManagerInner>,
    generation: u64,
    spawn: Option<SubagentSpawn>,
    notifier: SubagentNotifier,
    result: Result<Value, String>,
}

impl Drop for Completion {
    fn drop(&mut self) {
        let spawn = self.spawn.take().unwrap();
        self.manager.finish(self.generation, spawn.id);
        (self.notifier)(SubagentNotification {
            generation: self.generation,
            spawn,
            result: std::mem::replace(&mut self.result, Err(String::new())),
        });
    }
}

pub(super) fn execution_deadline(
    configured: Option<tokio::time::Instant>,
    timeout_secs: u32,
) -> Option<tokio::time::Instant> {
    let worker = (timeout_secs != 0).then(|| {
        tokio::time::Instant::now() + std::time::Duration::from_secs(u64::from(timeout_secs))
    });
    configured.into_iter().chain(worker).min()
}
