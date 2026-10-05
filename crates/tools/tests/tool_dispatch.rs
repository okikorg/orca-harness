//! `ToolDispatch`, the handle `bun_repl` dispatches nested calls through:
//! nested calls behave exactly like top-level calls through the kernel's
//! default dispatcher, and nesting cannot reach `bun_repl` or deadlock on
//! the parent's resources.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use orca_harness_core::{
    CancellationToken, Concurrency, Dispatcher, Extension, ExtensionError, ExtensionRegistry,
    FnTool, Subscriptions, Tool, ToolCall, ToolContext, ToolDecision, ToolError, ToolRegistry,
    ToolResult,
};
use orca_harness_tools::{ProgrammaticTools, ToolDispatch};
use serde_json::{json, Value};

fn ctx(call_id: &str) -> ToolContext {
    ToolContext {
        call_id: call_id.into(),
        tool_name: "bun_repl".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

fn echo(name: &str) -> Arc<dyn Tool> {
    Arc::new(FnTool::new(name, "", json!({}), |input, _| async move {
        Ok(input)
    }))
}

fn calls(names: &[(&str, Value)]) -> Vec<(String, Value)> {
    names
        .iter()
        .map(|(name, arguments)| (name.to_string(), arguments.clone()))
        .collect()
}

/// Denies `denied`, rewrites `rewritten`, wraps every output it sees after
/// execution, and records the ids it saw before and after.
#[derive(Default)]
struct Policy {
    before: Mutex<Vec<String>>,
    results: Mutex<Vec<String>>,
}

#[async_trait]
impl Extension for Policy {
    fn name(&self) -> &str {
        "policy"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none()
            .before_tool()
            .after_tool()
            .tool_result()
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        self.before.lock().unwrap().push(call.id.clone());
        Ok(match call.name.as_str() {
            "denied" => ToolDecision::Deny {
                reason: "blocked".into(),
            },
            "rewritten" => ToolDecision::Rewrite(json!({"rewritten": true})),
            _ => ToolDecision::Continue,
        })
    }

    async fn after_tool(
        &self,
        _call: &ToolCall,
        mut result: ToolResult,
    ) -> Result<ToolResult, ExtensionError> {
        result.output = json!({"after": result.output});
        Ok(result)
    }

    async fn tool_result(&self, result: &ToolResult) {
        self.results.lock().unwrap().push(result.call_id.clone());
    }
}

fn parity_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        echo("echo"),
        echo("denied"),
        echo("rewritten"),
        Arc::new(FnTool::new("fails", "", json!({}), |_, _| async {
            Err(ToolError::msg("boom"))
        })),
    ]
}

#[tokio::test]
async fn nested_calls_match_top_level_dispatch() {
    let policy = Arc::new(Policy::default());
    let batch = calls(&[
        ("echo", json!({"n": 1})),
        ("denied", json!({})),
        ("rewritten", json!({"n": 3})),
        ("fails", json!({})),
        ("missing", json!({})),
    ]);

    let mut registry = ToolRegistry::new();
    let mut extensions = ExtensionRegistry::new();
    for tool in parity_tools() {
        registry.register(tool);
    }
    extensions.register(policy.clone());
    let top_level = Dispatcher::new()
        .execute(
            batch
                .iter()
                .enumerate()
                .map(|(i, (name, arguments))| ToolCall {
                    id: format!("top{i}"),
                    name: name.clone(),
                    arguments: arguments.clone(),
                })
                .collect(),
            &registry,
            &extensions,
            &CancellationToken::new(),
            None,
            usize::MAX,
        )
        .await
        .unwrap();

    let dispatch = ToolDispatch::new(
        parity_tools(),
        [policy.clone() as Arc<dyn Extension>],
        ProgrammaticTools::new(),
    );
    let nested = dispatch.execute(&ctx("parent"), batch).await.unwrap();

    assert_eq!(nested.len(), top_level.len());
    for (nested, top) in nested.iter().zip(&top_level) {
        assert_eq!(nested.tool_name, top.tool_name);
        assert_eq!(nested.output, top.output, "{}", nested.tool_name);
        assert_eq!(nested.is_error, top.is_error, "{}", nested.tool_name);
    }
    assert_eq!(nested[0].output, json!({"after": {"n": 1}}));
    assert_eq!(nested[1].output, json!({"error": "denied: blocked"}));
    assert_eq!(nested[2].output, json!({"after": {"rewritten": true}}));
    assert_eq!(nested[3].output, json!({"after": {"error": "boom"}}));
    assert_eq!(nested[4].output, json!({"error": "unknown tool: missing"}));

    // The run's own extension instance saw the nested calls, under ids
    // that carry the parent's.
    let ids: Vec<String> = (1..=5).map(|n| format!("parent.ptc{n}")).collect();
    let nested_ids: Vec<String> = nested.iter().map(|r| r.call_id.clone()).collect();
    assert_eq!(nested_ids, ids);
    // `missing` never reaches before_tool, at the top level or nested.
    assert_eq!(policy.before.lock().unwrap()[4..], ids[..4]);
    assert_eq!(policy.results.lock().unwrap()[5..], ids[..]);
}

#[tokio::test]
async fn bun_repl_is_never_offered_to_nested_code() {
    let dispatch = ToolDispatch::new(
        [echo("bun_repl"), echo("echo")],
        [],
        ProgrammaticTools::new(),
    );
    let names: Vec<String> = dispatch.schemas().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["echo"]);
    let results = dispatch
        .execute(
            &ctx("parent"),
            calls(&[("bun_repl", json!({"action": "reset"}))]),
        )
        .await
        .unwrap();
    assert!(results[0].is_error);
    assert_eq!(
        results[0].output,
        json!({"error": "unknown tool: bun_repl"})
    );
}

#[tokio::test]
async fn visibility_is_read_live_and_hidden_tools_are_unknown() {
    let shown = Arc::new(AtomicBool::new(false));
    let visible = shown.clone();
    let policy = Arc::new(Policy::default());
    let dispatch = ToolDispatch::new(
        [echo("echo"), echo("deferred")],
        [policy.clone() as Arc<dyn Extension>],
        ProgrammaticTools::new().visibility(Arc::new(move |schema| {
            schema.name != "deferred" || visible.load(Ordering::SeqCst)
        })),
    );
    let names = |d: &ToolDispatch| -> Vec<String> {
        d.schemas().into_iter().map(|s| s.name).collect::<Vec<_>>()
    };
    assert_eq!(names(&dispatch), ["echo"]);
    let hidden = dispatch
        .execute(&ctx("p"), calls(&[("deferred", json!({}))]))
        .await
        .unwrap();
    assert_eq!(hidden[0].output, json!({"error": "unknown tool: deferred"}));
    // Like a top-level unknown tool, a hidden one never reaches policy.
    assert!(policy.before.lock().unwrap().is_empty());

    shown.store(true, Ordering::SeqCst);
    assert_eq!(names(&dispatch), ["echo", "deferred"]);
    let loaded = dispatch
        .execute(&ctx("p"), calls(&[("deferred", json!({"ok": 1}))]))
        .await
        .unwrap();
    assert_eq!(loaded[0].output, json!({"after": {"ok": 1}}));
    assert_eq!(loaded[0].call_id, "p.ptc2");
}

fn sleeper(dropped: Arc<AtomicUsize>, started: Arc<AtomicUsize>) -> Arc<dyn Tool> {
    struct DropFlag(Arc<AtomicUsize>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    Arc::new(FnTool::new("sleep", "", json!({}), move |_, _| {
        let flag = DropFlag(dropped.clone());
        let started = started.clone();
        async move {
            let _flag = flag;
            started.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(json!("woke"))
        }
    }))
}

#[tokio::test]
async fn the_run_deadline_interrupts_nested_calls_as_it_does_top_level_ones() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let tool = sleeper(dropped.clone(), Arc::default());
    let deadline = tokio::time::Instant::now() + Duration::from_millis(50);

    let mut registry = ToolRegistry::new();
    registry.register(tool.clone());
    let top_level = Dispatcher::new()
        .execute(
            vec![ToolCall {
                id: "top".into(),
                name: "sleep".into(),
                arguments: json!({}),
            }],
            &registry,
            &ExtensionRegistry::new(),
            &CancellationToken::new(),
            Some(deadline),
            usize::MAX,
        )
        .await
        .unwrap();

    let parent = ToolContext {
        deadline: Some(deadline),
        ..ctx("parent")
    };
    let dispatch = ToolDispatch::new([tool], [], ProgrammaticTools::new());
    let nested = dispatch
        .execute(&parent, calls(&[("sleep", json!({}))]))
        .await
        .unwrap();

    assert_eq!(nested[0].output, top_level[0].output);
    assert_eq!(
        nested[0].output,
        json!({"error": "run deadline exceeded during execution"})
    );
    assert!(!parent.cancellation.is_cancelled());
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn dropping_a_dispatch_interrupts_nested_calls_and_cancels_nothing() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(Finished::default());
    let dispatch = ToolDispatch::new(
        [sleeper(dropped.clone(), started.clone())],
        [finished.clone() as Arc<dyn Extension>],
        ProgrammaticTools::new(),
    );
    let parent = ctx("parent");
    // Two calls: one runs inline in the dispatching future, one in the
    // dispatcher's JoinSet. Both must stop when the future is dropped.
    let batch = calls(&[("sleep", json!({})), ("sleep", json!({}))]);
    let running = dispatch.execute(&parent, batch);
    tokio::select! {
        _ = running => panic!("the nested calls should still be running"),
        _ = async {
            while started.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        } => {}
    }
    for _ in 0..100 {
        if dropped.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    assert!(!parent.cancellation.is_cancelled());
    // Every nested call that started also reports a finish, as an error.
    for _ in 0..100 {
        if finished.0.lock().unwrap().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut seen = finished.0.lock().unwrap().clone();
    seen.sort();
    assert_eq!(
        seen,
        [
            ("parent.ptc1".to_string(), true),
            ("parent.ptc2".to_string(), true)
        ]
    );
}

/// Records every `tool_finished` it sees.
#[derive(Default)]
struct Finished(Mutex<Vec<(String, bool)>>);

#[async_trait]
impl Extension for Finished {
    fn name(&self) -> &str {
        "finished"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().tool_finished()
    }

    async fn tool_finished(&self, call_id: &str, _tool_name: &str, is_error: bool) {
        self.0.lock().unwrap().push((call_id.to_string(), is_error));
    }
}

fn keyed(name: &'static str, key: &'static str) -> Arc<dyn Tool> {
    Arc::new(
        FnTool::new(name, "", json!({}), |_, _| async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok(json!("done"))
        })
        .concurrency(move |_| Concurrency::Keyed(key.into())),
    )
}

/// A parent holding `key` that nests `nested` through `dispatch`.
fn parent(
    name: &'static str,
    key: &'static str,
    dispatch: ToolDispatch,
    nested: [&'static str; 2],
) -> Arc<dyn Tool> {
    Arc::new(
        FnTool::new(name, "", json!({}), move |_, ctx| {
            let dispatch = dispatch.clone();
            async move {
                let batch = nested.iter().map(|n| (n.to_string(), json!({}))).collect();
                let results = dispatch
                    .execute(&ctx, batch)
                    .await
                    .map_err(|e| ToolError::msg(e.to_string()))?;
                Ok(json!(results))
            }
        })
        .concurrency(move |_| Concurrency::Keyed(key.into())),
    )
}

#[tokio::test]
async fn parents_holding_keys_never_deadlock_on_their_nested_calls() {
    // A holds "a" and nests calls keyed "a" and "b"; B holds "b" and nests
    // "b" and "a". A shared key table that parents kept while suspended
    // would deadlock here; nested batches share nothing with the parent's.
    let dispatch = ToolDispatch::new(
        [keyed("a_tool", "a"), keyed("b_tool", "b")],
        [],
        ProgrammaticTools::new(),
    );
    for max_parallel in [1, 2, usize::MAX] {
        let mut registry = ToolRegistry::new();
        registry.register(parent("A", "a", dispatch.clone(), ["a_tool", "b_tool"]));
        registry.register(parent("B", "b", dispatch.clone(), ["b_tool", "a_tool"]));
        let results = tokio::time::timeout(
            Duration::from_secs(5),
            Dispatcher::new().execute(
                ["A", "B"]
                    .iter()
                    .map(|name| ToolCall {
                        id: format!("{name}{max_parallel}"),
                        name: name.to_string(),
                        arguments: json!({}),
                    })
                    .collect(),
                &registry,
                &ExtensionRegistry::new(),
                &CancellationToken::new(),
                None,
                max_parallel,
            ),
        )
        .await
        .expect("nested keyed calls deadlocked")
        .unwrap();
        for result in &results {
            assert!(!result.is_error, "{result:?}");
            for nested in result.output.as_array().unwrap() {
                assert_eq!(nested["output"], "done");
            }
        }
    }
}
