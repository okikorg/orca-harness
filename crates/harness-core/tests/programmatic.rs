use async_trait::async_trait;
use orca_harness_core::*;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
fn call(name: &str) -> ToolCall {
    ToolCall {
        id: format!("root_{name}"),
        name: name.into(),
        arguments: json!({}),
    }
}
fn nested(name: &str) -> ProgrammaticCall {
    ProgrammaticCall {
        name: name.into(),
        arguments: json!({}),
    }
}
struct TestTool {
    name: &'static str,
    concurrency: Concurrency,
    running: Arc<AtomicUsize>,
    max: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for TestTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.into(),
            description: "".into(),
            parameters: json!({}),
        }
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        self.concurrency.clone()
    }
    async fn call(&self, _: Value, _: &ToolContext) -> Result<Value, ToolError> {
        let n = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(n, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        Ok(json!(n))
    }
}
/// Each call seen: tool name, parent call id, call id.
type Seen = Arc<Mutex<Vec<(String, Option<String>, String)>>>;
struct Policy(Seen);
#[async_trait]
impl Extension for Policy {
    fn name(&self) -> &str {
        "policy"
    }
    async fn before_tool(&self, c: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        let i = current_tool_invocation().unwrap();
        self.0
            .lock()
            .unwrap()
            .push((c.id.clone(), i.parent_call_id, c.name.clone()));
        if c.name == "denied" {
            Ok(ToolDecision::Deny {
                reason: "blocked".into(),
            })
        } else {
            Ok(ToolDecision::Continue)
        }
    }
}
#[tokio::test]
async fn nested_policy_identity_visibility_and_single_slot_serial_do_not_deadlock() {
    let mut tools = ToolRegistry::new();
    let running = Arc::new(AtomicUsize::new(0));
    let max = Arc::new(AtomicUsize::new(0));
    for name in ["serial", "denied", "hidden"] {
        tools.register(Arc::new(TestTool {
            name,
            concurrency: Concurrency::Serial,
            running: running.clone(),
            max: max.clone(),
        }));
    }
    tools.register(Arc::new(FnTool::new(
        "orchestrate",
        "",
        json!({}),
        |_, ctx| async move {
            let results = ctx
                .dispatch_tools(vec![nested("serial"), nested("denied"), nested("hidden")])
                .await?;
            Ok(json!(results))
        },
    )));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut extensions = ExtensionRegistry::new();
    extensions.register(Arc::new(Policy(observed.clone())));
    let dispatcher = Dispatcher::new()
        .programmatic_tools(ProgrammaticTools::new().visibility(Arc::new(|s| s.name != "hidden")));
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        dispatcher.execute(
            vec![call("orchestrate")],
            &tools,
            &extensions,
            &CancellationToken::new(),
            None,
            1,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output[0].is_error);
    assert_eq!(output[0].output[0]["output"], 1);
    assert_eq!(output[0].output[1]["is_error"], true);
    assert_eq!(output[0].output[2]["is_error"], true);
    let seen = observed.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[1].1.as_deref(), Some("root_orchestrate"));
    assert!(seen[1].0.starts_with("ptc_"));
    assert_ne!(seen[1].0, seen[2].0);
}
#[tokio::test]
async fn nested_batches_share_parallel_limits_with_outer_siblings() {
    let mut tools = ToolRegistry::new();
    let running = Arc::new(AtomicUsize::new(0));
    let max = Arc::new(AtomicUsize::new(0));
    tools.register(Arc::new(TestTool {
        name: "work",
        concurrency: Concurrency::Parallel,
        running: running.clone(),
        max: max.clone(),
    }));
    tools.register(Arc::new(FnTool::new(
        "orchestrate",
        "",
        json!({}),
        |_, ctx| async move {
            Ok(json!(
                ctx.dispatch_tools((0..12).map(|_| nested("work")).collect())
                    .await?
            ))
        },
    )));
    let output = Dispatcher::new()
        .programmatic_tools(ProgrammaticTools::new())
        .execute(
            vec![call("orchestrate"), call("work")],
            &tools,
            &ExtensionRegistry::new(),
            &CancellationToken::new(),
            None,
            2,
        )
        .await
        .unwrap();
    assert!(output.iter().all(|r| !r.is_error));
    assert!(max.load(Ordering::SeqCst) <= 2);
    assert_eq!(running.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn cancellation_drops_nested_client_wait_and_releases_scheduler() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let mut tools = ToolRegistry::new();
    struct DropFlag(Arc<AtomicUsize>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let d = dropped.clone();
    let n = started.clone();
    tools.register(Arc::new(FnTool::new(
        "client",
        "",
        json!({}),
        move |_, _| {
            let d = d.clone();
            let n = n.clone();
            async move {
                let _flag = DropFlag(d);
                n.notify_one();
                std::future::pending::<()>().await;
                Ok(json!(null))
            }
        },
    )));
    tools.register(Arc::new(FnTool::new(
        "orchestrate",
        "",
        json!({}),
        |_, ctx| async move { Ok(json!(ctx.dispatch_tools(vec![nested("client")]).await?)) },
    )));
    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        started.notified().await;
        cancel.cancel();
    });
    let out = Dispatcher::new()
        .programmatic_tools(ProgrammaticTools::new())
        .execute(
            vec![call("orchestrate")],
            &tools,
            &ExtensionRegistry::new(),
            &token,
            None,
            1,
        )
        .await
        .unwrap();
    assert!(out[0].is_error);
    for _ in 0..20 {
        if dropped.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

struct Reentrant;
#[async_trait]
impl Tool for Reentrant {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "bun_repl".into(),
            description: "".into(),
            parameters: json!({}),
        }
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Keyed("bun_repl".into())
    }
    async fn call(&self, _: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        Ok(json!(
            ctx.dispatch_tools(vec![ProgrammaticCall {
                name: "bun_repl".into(),
                arguments: json!({"action":"reset"})
            }])
            .await?
        ))
    }
}
#[tokio::test]
async fn nested_interpreter_reset_is_rejected_without_deadlock() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Reentrant));
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        Dispatcher::new()
            .programmatic_tools(ProgrammaticTools::new())
            .execute(
                vec![call("bun_repl")],
                &tools,
                &ExtensionRegistry::new(),
                &CancellationToken::new(),
                None,
                1,
            ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!out[0].is_error);
    assert_eq!(out[0].output[0]["is_error"], true);
    assert!(out[0].output.to_string().contains("ancestor resource"));
}
