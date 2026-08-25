//! The Dispatcher: a first-class kernel component, because execution
//! semantics affect correctness.
//!
//! It resolves tools, preserves call/result identity, classifies
//! concurrency, executes parallel-safe calls concurrently, serializes
//! conflicting calls, enforces maximum parallelism, propagates
//! cancellation and deadlines, normalizes failures, and restores
//! deterministic model-visible ordering.
//!
//! Interruptions are per-call, not per-batch: when cancellation or the
//! run deadline fires mid-dispatch, calls that finished keep their real
//! results and only the interrupted calls carry an error saying what
//! stopped them and when. The run-level consequence (ending the loop)
//! belongs to the agent loop, which checks the token and deadline before
//! each model step. Dropping completed work would force the host to
//! stamp every call with a synthetic error the transcript cannot
//! distinguish from failure.
//!
//! Scheduling model:
//! - every running call holds one semaphore permit (`max_parallel_tools`);
//! - `Parallel` calls take a read lock, `Serial` calls a write lock on a
//!   shared RwLock, making serial calls exclusive against everything;
//! - calls sharing a `Keyed` key form one chain executed in call order;
//! - lock acquisition order is always permit → RwLock, so lock waiters
//!   always hold a permit and the pair cannot deadlock.
//!
//! Extension hooks stay deterministic except for the explicitly live
//! `tool_finished` observation: `before_tool` hooks run sequentially in call
//! order before execution; `tool_finished` runs in the completing task;
//! `after_tool` / `tool_result` run sequentially in call order after all
//! results are collected. Only tool execution itself (wrapped by
//! `around_tool`) and live completion observation are concurrent.
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
        let mut calls = calls;
        // After this phase, a call's arguments are observable only through
        // around_tool / after_tool hooks. With neither subscribed, each
        // executing call's arguments MOVE into its job — a large payload
        // (1 MiB write_file content) is never deep-copied.
        let strip_arguments = extensions.around_tool_chain().is_empty()
            && extensions.after_tool_subscribers().is_empty();
        // Results land in their original call slot; `executed` marks slots
        // whose tool actually ran (denied/unresolved calls skip after_tool).
        let mut slots: Vec<Option<ToolResult>> = (0..n).map(|_| None).collect();
        let mut executed = vec![false; n];

        // Phase 1 — deterministic pre-hooks, in call order.
        let mut grouping = Grouping::default();
        let mut has_serial = false;
        let mut job_count = 0usize;
        for (index, call) in calls.iter_mut().enumerate() {
            let Some(tool) = tools.get(&call.name) else {
                slots[index] = Some(ToolResult::error(
                    call,
                    format!("unknown tool: {}", call.name),
                ));
                continue;
            };

            let mut rewritten = None;
            let mut denied = None;
            for ext in extensions.before_tool_subscribers() {
                match ext.before_tool(call).await? {
                    ToolDecision::Continue => {}
                    ToolDecision::Rewrite(new_input) => rewritten = Some(new_input),
                    ToolDecision::Deny { reason } => {
                        denied = Some(reason);
                        break;
                    }
                }
            }
            if let Some(reason) = denied {
                let result = ToolResult::error(call, format!("denied: {reason}"));
                for ext in extensions.tool_finished_subscribers() {
                    ext.tool_finished(&call.id, &call.name, true).await;
                }
                slots[index] = Some(result);
                continue;
            }
            let input = match rewritten {
                Some(new_input) => new_input,
                None if strip_arguments => std::mem::take(&mut call.arguments),
                None => call.arguments.clone(),
            };

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
        let calls: Arc<[ToolCall]> = calls.into();

        // Phase 2 — concurrent execution. Synchronization is only
        // constructed when it can actually constrain this batch.
        let max_parallel = max_parallel.max(1);
        let shared = Arc::new(SharedExecution {
            semaphore: (job_count > max_parallel).then(|| Semaphore::new(max_parallel)),
            exclusivity: has_serial.then(RwLock::default),
            calls,
            around_chain: extensions.around_tool_chain().to_vec(),
            tool_finished: extensions.tool_finished_subscribers().to_vec(),
            cancellation: cancellation.clone(),
            deadline,
        });

        if job_count > 0 {
            // Multi-key merges can leave drained chains behind; drop them
            // so they neither spawn no-op tasks nor claim the inline slot.
            grouping.chains.retain(|chain| !chain.is_empty());
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

            let mut join_set: JoinSet<Chain> = JoinSet::new();
            for job in grouping.singles.drain(..) {
                let shared = shared.clone();
                join_set.spawn(async move {
                    let index = job.index;
                    let result = shared.run_job(job).await;
                    Chain::One(index, result)
                });
            }
            for chain in grouping.chains.drain(..) {
                let shared = shared.clone();
                join_set.spawn(async move {
                    let mut out = Vec::with_capacity(chain.len());
                    for job in chain {
                        let index = job.index;
                        out.push((index, shared.run_job(job).await));
                    }
                    Chain::Many(out)
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
                let result = shared.run_job(job).await;
                executed[index] = true;
                slots[index] = Some(result);
            }

            while let Some(joined) = join_set.join_next().await {
                match joined {
                    Ok(Chain::One(index, result)) => {
                        executed[index] = true;
                        slots[index] = Some(result);
                    }
                    Ok(Chain::Many(pairs)) => {
                        for (index, result) in pairs {
                            executed[index] = true;
                            slots[index] = Some(result);
                        }
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
    tool_finished: Vec<Arc<dyn crate::extension::Extension>>,
    cancellation: CancellationToken,
    deadline: Option<Instant>,
}

impl SharedExecution {
    /// Execute one job to a definite result. Interruptions (cancellation,
    /// the run deadline) land in this call's own result slot as an error
    /// naming what stopped it and whether it had started, so completed
    /// siblings keep their outputs.
    async fn run_job(&self, job: Job) -> ToolResult {
        let call = &self.calls[job.index];
        // Permit first, then lock — see module docs for why this order
        // is deadlock-free.
        let _permit = match &self.semaphore {
            Some(semaphore) => match self.guarded(semaphore.acquire()).await {
                Ok(Ok(permit)) => Some(permit),
                Ok(Err(closed)) => {
                    return self
                        .finish(call, ToolResult::error(call, closed.to_string()))
                        .await
                }
                Err(err) => {
                    return self
                        .finish(
                            call,
                            ToolResult::error(call, interrupted(&err, "before execution")),
                        )
                        .await
                }
            },
            None => None,
        };
        let _lock: Option<Hold<'_>> = match &self.exclusivity {
            Some(lock) if job.exclusive => match self.guarded(lock.write()).await {
                Ok(guard) => Some(Hold::Exclusive(guard)),
                Err(err) => {
                    return self
                        .finish(
                            call,
                            ToolResult::error(call, interrupted(&err, "before execution")),
                        )
                        .await
                }
            },
            Some(lock) => match self.guarded(lock.read()).await {
                Ok(guard) => Some(Hold::Shared(guard)),
                Err(err) => {
                    return self
                        .finish(
                            call,
                            ToolResult::error(call, interrupted(&err, "before execution")),
                        )
                        .await
                }
            },
            None => None,
        };

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
        let result = match self.guarded(next.run(job.input)).await {
            Ok(Ok(output)) => ToolResult::ok(call, output),
            Ok(Err(err)) => ToolResult::error(call, err.message),
            Err(err) => ToolResult::error(call, interrupted(&err, "during execution")),
        };
        self.finish(call, result).await
    }

    async fn finish(&self, call: &ToolCall, result: ToolResult) -> ToolResult {
        for ext in &self.tool_finished {
            ext.tool_finished(&call.id, &call.name, result.is_error)
                .await;
        }
        result
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

/// The model-visible message for a call the kernel had to interrupt.
/// `phase` tells the model whether the tool ran at all.
fn interrupted(err: &HarnessError, phase: &str) -> String {
    match err {
        HarnessError::Cancelled => format!("cancelled {phase}"),
        HarnessError::DeadlineExceeded => format!("run deadline exceeded {phase}"),
        other => format!("{other} {phase}"),
    }
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
            Concurrency::Keys(keys) if keys.is_empty() => self.singles.push(job),
            Concurrency::Keys(keys) => {
                // Every chain any key belongs to must serialize with this
                // job: merge them into one (per-key call order survives —
                // each chain's internal order is kept, and this job is
                // appended after all of them).
                let mut slots: Vec<usize> = keys
                    .iter()
                    .filter_map(|k| self.keyed.get(k).copied())
                    .collect();
                slots.sort_unstable();
                slots.dedup();
                let target = match slots.split_first() {
                    None => {
                        self.chains.push(Vec::new());
                        self.chains.len() - 1
                    }
                    Some((&first, rest)) => {
                        for &slot in rest {
                            let bridged = std::mem::take(&mut self.chains[slot]);
                            self.chains[first].extend(bridged);
                        }
                        if !rest.is_empty() {
                            for slot in self.keyed.values_mut() {
                                if rest.contains(slot) {
                                    *slot = first;
                                }
                            }
                        }
                        first
                    }
                };
                for key in keys {
                    self.keyed.insert(key, target);
                }
                self.chains[target].push(job);
            }
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
