//! The Dispatcher: a first-class kernel component, because execution
//! semantics affect correctness.
//!
//! It resolves tools, preserves call/result identity, classifies
//! concurrency, executes parallel-safe calls concurrently, serializes
//! conflicting calls, enforces maximum parallelism, propagates
//! cancellation and deadlines, normalizes failures, and restores
//! deterministic model-visible ordering.
//!
//! Scheduling model:
//! - every running call holds one semaphore permit (`max_parallel_tools`);
//! - `Parallel` calls take a read lock, `Serial` calls a write lock on a
//!   shared RwLock, making serial calls exclusive against everything;
//! - calls sharing a `Keyed` key form one chain executed in call order;
//! - lock acquisition order is always permit → RwLock, so lock waiters
//!   always hold a permit and the pair cannot deadlock.
//!
//! Extension hooks stay deterministic: all `before_tool` hooks run
//! sequentially in call order before any execution starts, and
//! `after_tool` / `tool_result` hooks run sequentially in call order after
//! all results are collected. Only tool execution itself (wrapped by
//! `around_tool`) is concurrent.
//!
//! Fan-out hot path: synchronization that cannot matter is elided — the
//! semaphore is skipped when the batch fits under `max_parallel_tools`
//! and the RwLock is skipped when the batch contains no `Serial` call.
//! One unit of every batch runs inline in the dispatching task after the
//! rest are spawned (the dispatcher would otherwise idle in `join_next`),
//! so a single-call batch never crosses threads at all. Semantics are
//! unchanged; only the overhead goes.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{RwLock, Semaphore};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::error::HarnessError;
use crate::extension::{ExtensionRegistry, Next, ToolDecision};
use crate::tool::{Concurrency, Tool, ToolCall, ToolResult};

#[derive(Debug, Clone, Default)]
pub struct Dispatcher;

/// One executable unit after resolution and policy. The call itself is
/// addressed by index into the shared batch to avoid cloning `ToolCall`s
/// (two Strings + a Value) onto every task.
struct Job {
    index: usize,
    input: Value,
    tool: Arc<dyn Tool>,
    exclusive: bool,
}

impl Dispatcher {
    pub fn new() -> Self {
        Self
    }

    pub async fn execute(
        &self,
        calls: Vec<ToolCall>,
        tools: &crate::tool::ToolRegistry,
        extensions: &ExtensionRegistry,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
        max_parallel: usize,
    ) -> Result<Vec<ToolResult>, HarnessError> {
        validate_pairing(&calls)?;

        let n = calls.len();
        let calls: Arc<[ToolCall]> = calls.into();
        // Results land in their original call slot; `executed` marks slots
        // whose tool actually ran (denied/unresolved calls skip after_tool).
        let mut slots: Vec<Option<ToolResult>> = (0..n).map(|_| None).collect();
        let mut executed = vec![false; n];

        // Phase 1 — deterministic pre-hooks, in call order.
        let mut grouping = Grouping::default();
        let mut has_serial = false;
        let mut job_count = 0usize;
        for (index, call) in calls.iter().enumerate() {
            let Some(tool) = tools.get(&call.name) else {
                slots[index] = Some(ToolResult::error(
                    call,
                    format!("unknown tool: {}", call.name),
                ));
                continue;
            };

            let mut input = call.arguments.clone();
            let mut denied = None;
            for ext in extensions.before_tool_subscribers() {
                match ext.before_tool(call).await? {
                    ToolDecision::Continue => {}
                    ToolDecision::Rewrite(new_input) => input = new_input,
                    ToolDecision::Deny { reason } => {
                        denied = Some(reason);
                        break;
                    }
                }
            }
            if let Some(reason) = denied {
                slots[index] = Some(ToolResult::error(call, format!("denied: {reason}")));
                continue;
            }

            let concurrency = tool.concurrency(&input);
            has_serial |= matches!(concurrency, Concurrency::Serial);
            job_count += 1;
            grouping.add(
                Job {
                    index,
                    input,
                    tool: tool.clone(),
                    exclusive: false,
                },
                concurrency,
            );
        }

        // Phase 2 — concurrent execution. Synchronization is only
        // constructed when it can actually constrain this batch.
        let max_parallel = max_parallel.max(1);
        let shared = Arc::new(SharedExecution {
            semaphore: (job_count > max_parallel).then(|| Semaphore::new(max_parallel)),
            exclusivity: has_serial.then(RwLock::default),
            calls,
            around_chain: extensions.around_tool_chain().to_vec(),
            cancellation: cancellation.clone(),
            deadline,
        });

        if job_count > 0 {
            // Hold back one unit to run inline in this task (prefer a
            // lone single; a chain otherwise): the dispatcher would only
            // idle in `join_next`, and a single-call batch then never
            // pays a spawn or a cross-thread handoff at all.
            let inline_single = grouping.singles.pop();
            let inline_chain = if inline_single.is_none() {
                grouping.chains.pop()
            } else {
                None
            };

            let mut join_set: JoinSet<Result<Chain, HarnessError>> = JoinSet::new();
            for job in grouping.singles.drain(..) {
                let shared = shared.clone();
                join_set.spawn(async move {
                    let index = job.index;
                    let result = shared.run_job(job).await?;
                    Ok(Chain::One(index, result))
                });
            }
            for chain in grouping.chains.drain(..) {
                let shared = shared.clone();
                join_set.spawn(async move {
                    let mut out = Vec::with_capacity(chain.len());
                    for job in chain {
                        let index = job.index;
                        out.push((index, shared.run_job(job).await?));
                    }
                    Ok(Chain::Many(out))
                });
            }

            let mut failure: Option<HarnessError> = None;
            let inline_jobs = match (inline_single, inline_chain) {
                (Some(job), _) => vec![job],
                (None, Some(chain)) => chain,
                (None, None) => Vec::new(),
            };
            for job in inline_jobs {
                let index = job.index;
                match shared.run_job(job).await {
                    Ok(result) => {
                        executed[index] = true;
                        slots[index] = Some(result);
                    }
                    Err(err) => {
                        failure = Some(err);
                        join_set.abort_all();
                        break;
                    }
                }
            }

            while let Some(joined) = join_set.join_next().await {
                match joined {
                    Ok(Ok(Chain::One(index, result))) => {
                        executed[index] = true;
                        slots[index] = Some(result);
                    }
                    Ok(Ok(Chain::Many(pairs))) => {
                        for (index, result) in pairs {
                            executed[index] = true;
                            slots[index] = Some(result);
                        }
                    }
                    Ok(Err(err)) => {
                        failure.get_or_insert(err);
                        join_set.abort_all();
                    }
                    Err(join_err) if join_err.is_cancelled() => {}
                    Err(join_err) => {
                        failure.get_or_insert(HarnessError::Internal(join_err.to_string()));
                        join_set.abort_all();
                    }
                }
            }
            if let Some(err) = failure {
                return Err(err);
            }
        }

        // Phase 3 — deterministic post-hooks, in original call order.
        let mut results = Vec::with_capacity(n);
        for (index, slot) in slots.into_iter().enumerate() {
            let mut result = slot.ok_or_else(|| {
                HarnessError::Internal(format!("missing result for call index {index}"))
            })?;
            if executed[index] {
                for ext in extensions.after_tool_subscribers() {
                    result = ext.after_tool(&shared.calls[index], result).await?;
                }
            }
            results.push(result);
        }
        for result in &results {
            for ext in extensions.tool_result_subscribers() {
                ext.tool_result(result).await;
            }
        }

        Ok(results)
    }
}

enum Chain {
    One(usize, ToolResult),
    Many(Vec<(usize, ToolResult)>),
}

/// State shared by all execution tasks of one dispatch. `semaphore` and
/// `exclusivity` are only present when this batch can actually contend
/// on them.
struct SharedExecution {
    semaphore: Option<Semaphore>,
    exclusivity: Option<RwLock<()>>,
    calls: Arc<[ToolCall]>,
    around_chain: Vec<Arc<dyn crate::extension::Extension>>,
    cancellation: CancellationToken,
    deadline: Option<Instant>,
}

impl SharedExecution {
    async fn run_job(&self, job: Job) -> Result<ToolResult, HarnessError> {
        // Permit first, then lock — see module docs for why this order
        // is deadlock-free.
        let _permit = match &self.semaphore {
            Some(semaphore) => Some(
                self.guarded(semaphore.acquire())
                    .await?
                    .map_err(|e| HarnessError::Internal(e.to_string()))?,
            ),
            None => None,
        };
        let _lock: Option<Hold<'_>> = match &self.exclusivity {
            Some(lock) if job.exclusive => Some(Hold::Exclusive(self.guarded(lock.write()).await?)),
            Some(lock) => Some(Hold::Shared(self.guarded(lock.read()).await?)),
            None => None,
        };

        let call = &self.calls[job.index];
        let ctx = crate::tool::ToolContext {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            cancellation: self.cancellation.clone(),
            deadline: self.deadline,
        };
        let next = Next {
            chain: &self.around_chain,
            call,
            tool: job.tool.as_ref(),
            ctx: &ctx,
        };
        let outcome = self.guarded(next.run(job.input)).await?;
        Ok(match outcome {
            Ok(output) => ToolResult::ok(call, output),
            Err(err) => ToolResult::error(call, err.message),
        })
    }

    /// Race a future against cancellation and the run deadline.
    async fn guarded<F: Future>(&self, fut: F) -> Result<F::Output, HarnessError> {
        match self.deadline {
            None => {
                tokio::select! {
                    biased;
                    _ = self.cancellation.cancelled() => Err(HarnessError::Cancelled),
                    out = fut => Ok(out),
                }
            }
            Some(deadline) => {
                tokio::select! {
                    biased;
                    _ = self.cancellation.cancelled() => Err(HarnessError::Cancelled),
                    out = tokio::time::timeout_at(deadline, fut) => {
                        out.map_err(|_| HarnessError::DeadlineExceeded)
                    }
                }
            }
        }
    }
}

enum Hold<'a> {
    Shared(#[allow(dead_code)] tokio::sync::RwLockReadGuard<'a, ()>),
    Exclusive(#[allow(dead_code)] tokio::sync::RwLockWriteGuard<'a, ()>),
}

/// Groups jobs so that each chain runs sequentially in call order while
/// singles and distinct chains run concurrently. `Parallel` jobs stay in
/// `singles` (spawned directly, no per-job Vec); `Keyed`/`Serial` jobs
/// form chains.
#[derive(Default)]
struct Grouping {
    singles: Vec<Job>,
    chains: Vec<Vec<Job>>,
    keyed: HashMap<String, usize>,
    serial: Option<usize>,
}

impl Grouping {
    fn add(&mut self, mut job: Job, concurrency: Concurrency) {
        match concurrency {
            Concurrency::Parallel => self.singles.push(job),
            Concurrency::Keyed(key) => match self.keyed.get(&key) {
                Some(&slot) => self.chains[slot].push(job),
                None => {
                    self.keyed.insert(key, self.chains.len());
                    self.chains.push(vec![job]);
                }
            },
            Concurrency::Serial => {
                job.exclusive = true;
                match self.serial {
                    Some(slot) => self.chains[slot].push(job),
                    None => {
                        self.serial = Some(self.chains.len());
                        self.chains.push(vec![job]);
                    }
                }
            }
        }
    }
}

fn validate_pairing(calls: &[ToolCall]) -> Result<(), HarnessError> {
    let mut seen = std::collections::HashSet::with_capacity(calls.len());
    for call in calls {
        if call.id.is_empty() {
            return Err(HarnessError::InvalidToolCall(format!(
                "tool call '{}' has an empty id",
                call.name
            )));
        }
        if !seen.insert(call.id.as_str()) {
            return Err(HarnessError::InvalidToolCall(format!(
                "duplicate tool call id '{}'",
                call.id
            )));
        }
    }
    Ok(())
}
