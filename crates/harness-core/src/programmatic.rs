//! Opt-in reentrant scheduling for orchestration tools. Suspended parents retain
//! their resource keys but release their execution slot and exclusivity. Nested
//! calls cannot acquire an ancestor key, so an interpreter cannot reset itself.
use crate::{
    CancellationToken, Concurrency, ExtensionRegistry, HarnessError, Next, ToolCall, ToolContext,
    ToolDecision, ToolError, ToolRegistry, ToolResult, ToolSchema,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::Notify;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Decides whether a schema is visible to programmatic callers.
pub type ToolVisibility = Arc<dyn Fn(&ToolSchema) -> bool + Send + Sync>;

#[derive(Clone, Default)]
pub struct ProgrammaticTools {
    visibility: Option<ToolVisibility>,
}
impl std::fmt::Debug for ProgrammaticTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgrammaticTools").finish_non_exhaustive()
    }
}
impl ProgrammaticTools {
    pub fn new() -> Self {
        Self::default()
    }
    /// Re-evaluated before every nested call and catalog request. The host can
    /// share its deferred discovery state here without exposing hidden schemas.
    pub fn visibility(mut self, visibility: ToolVisibility) -> Self {
        self.visibility = Some(visibility);
        self
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgrammaticCall {
    pub name: String,
    pub arguments: Value,
}
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub call_id: String,
    pub parent_call_id: Option<String>,
}
tokio::task_local! { static INVOCATION: ToolInvocation; static ACTIVE: Arc<Active>; }
/// Available during policy, execution and result hooks, including nested calls.
pub fn current_tool_invocation() -> Option<ToolInvocation> {
    INVOCATION.try_with(Clone::clone).ok()
}

#[derive(Default)]
struct State {
    active: usize,
    exclusive: bool,
    keys: HashSet<String>,
}
#[derive(Default)]
struct Scheduler {
    state: Mutex<State>,
    changed: Notify,
}
struct Lease {
    scheduler: Arc<Scheduler>,
    keys: Vec<String>,
    exclusive: bool,
    running: bool,
}
impl Lease {
    fn suspend(&mut self) {
        if self.running {
            let mut s = self.scheduler.state.lock().unwrap();
            s.active -= 1;
            if self.exclusive {
                s.exclusive = false;
            }
            self.running = false;
            drop(s);
            self.scheduler.changed.notify_waiters();
        }
    }
    async fn resume(&mut self, max: usize) {
        loop {
            let notified = self.scheduler.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut s = self.scheduler.state.lock().unwrap();
                if s.active < max && !s.exclusive && (!self.exclusive || s.active == 0) {
                    s.active += 1;
                    s.exclusive = self.exclusive;
                    self.running = true;
                    return;
                }
            }
            notified.await;
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut s = self.scheduler.state.lock().unwrap();
        if self.running {
            s.active -= 1;
            if self.exclusive {
                s.exclusive = false;
            }
        }
        for k in &self.keys {
            s.keys.remove(k);
        }
        drop(s);
        self.scheduler.changed.notify_waiters();
    }
}
impl Scheduler {
    async fn acquire(self: &Arc<Self>, keys: Vec<String>, exclusive: bool, max: usize) -> Lease {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut s = self.state.lock().unwrap();
                if s.active < max
                    && !s.exclusive
                    && (!exclusive || s.active == 0)
                    && keys.iter().all(|k| !s.keys.contains(k))
                {
                    s.active += 1;
                    s.exclusive = exclusive;
                    s.keys.extend(keys.iter().cloned());
                    return Lease {
                        scheduler: self.clone(),
                        keys,
                        exclusive,
                        running: true,
                    };
                }
            }
            notified.await;
        }
    }
}
pub(crate) struct Runtime {
    tools: ToolRegistry,
    extensions: ExtensionRegistry,
    config: ProgrammaticTools,
    scheduler: Arc<Scheduler>,
    max: usize,
    ids: Mutex<HashSet<String>>,
    cancellation: CancellationToken,
    deadline: Option<tokio::time::Instant>,
}
struct Active {
    runtime: Arc<Runtime>,
    lease: tokio::sync::Mutex<Lease>,
    call_id: String,
    ancestor_keys: Vec<String>,
    ancestor_calls: Vec<String>,
    cancellation: CancellationToken,
}
impl ToolContext {
    pub fn programmatic_tools_enabled(&self) -> bool {
        ACTIVE
            .try_with(|a| a.call_id == self.call_id)
            .unwrap_or(false)
    }
    pub fn parent_call_id(&self) -> Option<String> {
        current_tool_invocation().and_then(|i| i.parent_call_id)
    }
    pub fn programmatic_tool_schemas(&self) -> Result<Vec<ToolSchema>, ToolError> {
        let active = active(self)?;
        Ok(active
            .runtime
            .tools
            .schemas()
            .into_iter()
            .filter(|s| !active.ancestor_calls.contains(&s.name) && active.runtime.visible(s))
            .collect())
    }
    /// Dispatch through the current run's registry, policy and scheduler. IDs
    /// are generated by the host. Dropping this future cancels its parent tool.
    pub async fn dispatch_tools(
        &self,
        calls: Vec<ProgrammaticCall>,
    ) -> Result<Vec<ToolResult>, ToolError> {
        let active = active(self)?;
        if calls.is_empty() || calls.len() > 64 {
            return Err(ToolError::msg("Expected 1..64 programmatic calls"));
        }
        let mut lease = active.lease.lock().await;
        lease.suspend();
        struct CancelOnDrop(Option<CancellationToken>);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                if let Some(t) = self.0.take() {
                    t.cancel();
                }
            }
        }
        let mut cancel = CancelOnDrop(Some(active.cancellation.clone()));
        let calls = calls
            .into_iter()
            .map(|c| ToolCall {
                id: active.runtime.next_id(),
                name: c.name,
                arguments: c.arguments,
            })
            .collect();
        let result = active
            .runtime
            .clone()
            .dispatch(
                calls,
                Some(active.call_id.clone()),
                active.ancestor_keys.clone(),
                active.ancestor_calls.clone(),
                active.cancellation.clone(),
            )
            .await;
        guarded(
            &active.cancellation,
            active.runtime.deadline,
            lease.resume(active.runtime.max),
        )
        .await
        .map_err(|e| ToolError::msg(e.to_string()))?;
        cancel.0 = None;
        result.map_err(|e| ToolError::msg(e.to_string()))
    }
}
fn active(ctx: &ToolContext) -> Result<Arc<Active>, ToolError> {
    ACTIVE
        .try_with(Clone::clone)
        .ok()
        .filter(|a| a.call_id == ctx.call_id)
        .ok_or_else(|| ToolError::msg("Programmatic tools are not enabled for this invocation"))
}
impl Runtime {
    pub(crate) fn new(
        tools: &ToolRegistry,
        extensions: &ExtensionRegistry,
        config: ProgrammaticTools,
        max: usize,
        cancellation: &CancellationToken,
        deadline: Option<tokio::time::Instant>,
    ) -> Arc<Self> {
        Arc::new(Self {
            tools: tools.clone(),
            extensions: extensions.clone(),
            config,
            scheduler: Arc::default(),
            max: max.max(1),
            ids: Mutex::default(),
            cancellation: cancellation.clone(),
            deadline,
        })
    }
    fn visible(&self, schema: &ToolSchema) -> bool {
        self.config.visibility.as_ref().is_none_or(|f| f(schema))
    }
    fn next_id(&self) -> String {
        loop {
            let id = format!("ptc_{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
            if self.ids.lock().unwrap().insert(id.clone()) {
                return id;
            }
        }
    }
    #[allow(clippy::result_large_err)] // HarnessError is large crate-wide
    pub(crate) async fn execute(
        self: Arc<Self>,
        calls: Vec<ToolCall>,
    ) -> Result<Vec<ToolResult>, HarnessError> {
        self.ids
            .lock()
            .unwrap()
            .extend(calls.iter().map(|c| c.id.clone()));
        let token = self.cancellation.clone();
        self.dispatch(calls, None, Vec::new(), Vec::new(), token)
            .await
    }
    fn dispatch(
        self: Arc<Self>,
        calls: Vec<ToolCall>,
        parent: Option<String>,
        ancestor_keys: Vec<String>,
        ancestor_calls: Vec<String>,
        cancellation: CancellationToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<ToolResult>, HarnessError>> + Send>,
    > {
        Box::pin(async move {
            let mut slots = vec![None; calls.len()];
            let mut jobs = Vec::new();
            for (index, call) in calls.iter().enumerate() {
                let invocation = ToolInvocation {
                    call_id: call.id.clone(),
                    parent_call_id: parent.clone(),
                };
                let prepared = INVOCATION
                    .scope(invocation, async {
                        let Some(tool) = self.tools.get(&call.name).cloned() else {
                            return Ok(Err("Unknown tool".to_owned()));
                        };
                        if parent.is_some() && !self.visible(&tool.schema()) {
                            return Ok(Err("Tool is not visible".to_owned()));
                        }
                        let mut input = call.arguments.clone();
                        for ext in self.extensions.before_tool_subscribers() {
                            match ext.before_tool(call).await? {
                                ToolDecision::Continue => {}
                                ToolDecision::Rewrite(v) => input = v,
                                ToolDecision::Deny { reason } => {
                                    return Ok(Err(format!("denied: {reason}")))
                                }
                            }
                        }
                        if ancestor_calls.contains(&call.name) || ancestor_calls.len() >= 16 {
                            return Ok(Err(
                                "Nested call conflicts with an ancestor resource or depth limit"
                                    .to_owned(),
                            ));
                        }
                        let concurrency = tool.concurrency(&input);
                        let exclusive = matches!(concurrency, Concurrency::Serial);
                        let mut keys = match concurrency {
                            Concurrency::Keyed(k) => vec![k],
                            Concurrency::Keys(k) => k,
                            _ => Vec::new(),
                        };
                        keys.sort();
                        keys.dedup();
                        if keys.iter().any(|k| ancestor_keys.contains(k)) {
                            return Ok(Err(
                                "Nested call conflicts with an ancestor resource".to_owned()
                            ));
                        }
                        Ok::<_, HarnessError>(Ok((tool, input, keys, exclusive)))
                    })
                    .await?;
                match prepared {
                    Ok(job) => jobs.push((index, job)),
                    Err(reason) => slots[index] = Some((ToolResult::error(call, reason), false)),
                }
            }
            // Spawn in call order; scheduler grants all resources atomically so
            // blocked keys never consume a parallel slot needed by descendants.
            let mut set = tokio::task::JoinSet::new();
            let mut preceding: HashMap<String, tokio::sync::watch::Receiver<bool>> = HashMap::new();
            for (index, (tool, input, keys, exclusive)) in jobs {
                let (finished, receiver) = tokio::sync::watch::channel(false);
                let mut dependencies = Vec::new();
                let mut order_keys = keys.clone();
                if exclusive {
                    order_keys.push("\0serial".into());
                }
                for key in order_keys {
                    if let Some(previous) = preceding.insert(key, receiver.clone()) {
                        dependencies.push(previous);
                    }
                }
                let runtime = self.clone();
                let call = calls[index].clone();
                let parent = parent.clone();
                let mut ancestors = ancestor_keys.clone();
                ancestors.extend(keys.clone());
                let token = cancellation.child_token();
                let mut lineage = ancestor_calls.clone();
                lineage.push(call.name.clone());
                set.spawn(async move {
                    struct Finished(tokio::sync::watch::Sender<bool>);
                    impl Drop for Finished {
                        fn drop(&mut self) {
                            let _ = self.0.send(true);
                        }
                    }
                    let _finished = Finished(finished);
                    let invocation = ToolInvocation {
                        call_id: call.id.clone(),
                        parent_call_id: parent,
                    };
                    let result = INVOCATION
                        .scope(invocation, async {
                            let execution = async {
                                for mut dependency in dependencies {
                                    while !*dependency.borrow_and_update() {
                                        if dependency.changed().await.is_err() {
                                            break;
                                        }
                                    }
                                }
                                let lease = runtime
                                    .scheduler
                                    .acquire(keys, exclusive, runtime.max)
                                    .await;
                                let active = Arc::new(Active {
                                    runtime: runtime.clone(),
                                    lease: tokio::sync::Mutex::new(lease),
                                    call_id: call.id.clone(),
                                    ancestor_keys: ancestors,
                                    ancestor_calls: lineage,
                                    cancellation: token.clone(),
                                });
                                let ctx = ToolContext {
                                    call_id: call.id.clone(),
                                    tool_name: call.name.clone(),
                                    cancellation: token.clone(),
                                    deadline: runtime.deadline,
                                };
                                ACTIVE
                                    .scope(active, async {
                                        let next = Next {
                                            chain: runtime.extensions.around_tool_chain(),
                                            call: &call,
                                            tool: tool.as_ref(),
                                            ctx: &ctx,
                                        };
                                        next.run(input).await
                                    })
                                    .await
                            };
                            let result = match guarded(&token, runtime.deadline, execution).await {
                                Ok(Ok(v)) => ToolResult::ok(&call, v),
                                Ok(Err(e)) => ToolResult::error(&call, e.message),
                                Err(e) => ToolResult::error(&call, e.to_string()),
                            };
                            for ext in runtime.extensions.tool_finished_subscribers() {
                                ext.tool_finished(&call.id, &call.name, result.is_error)
                                    .await;
                            }
                            result
                        })
                        .await;
                    (index, result)
                });
            }
            while let Some(out) = set.join_next().await {
                let (index, result) = out.map_err(|e| HarnessError::Internal(e.to_string()))?;
                slots[index] = Some((result, true));
            }
            let mut results = Vec::new();
            for (index, slot) in slots.into_iter().enumerate() {
                let (mut result, executed) = slot.expect("every call resolved");
                INVOCATION
                    .scope(
                        ToolInvocation {
                            call_id: calls[index].id.clone(),
                            parent_call_id: parent.clone(),
                        },
                        async {
                            if executed {
                                for ext in self.extensions.after_tool_subscribers() {
                                    result = ext.after_tool(&calls[index], result.clone()).await?;
                                }
                            } else {
                                for ext in self.extensions.tool_finished_subscribers() {
                                    ext.tool_finished(
                                        &result.call_id,
                                        &result.tool_name,
                                        result.is_error,
                                    )
                                    .await;
                                }
                            }
                            for ext in self.extensions.tool_result_subscribers() {
                                ext.tool_result(&result).await;
                            }
                            Ok::<(), HarnessError>(())
                        },
                    )
                    .await?;
                results.push(result);
            }
            Ok(results)
        })
    }
}
#[allow(clippy::result_large_err)] // HarnessError is large crate-wide
async fn guarded<F: std::future::Future>(
    token: &CancellationToken,
    deadline: Option<tokio::time::Instant>,
    future: F,
) -> Result<F::Output, HarnessError> {
    tokio::select! { biased; _=token.cancelled()=>Err(HarnessError::Cancelled),out=async {match deadline {Some(d)=>tokio::time::timeout_at(d,future).await.map_err(|_|HarnessError::DeadlineExceeded),None=>Ok(future.await)}}=>out }
}
