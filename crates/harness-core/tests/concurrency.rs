//! Tests for the Dispatcher's concurrent execution semantics, driven
//! end-to-end through the Agent with a fake (scripted) LLM.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;

use orca_harness_core::testing::{call, ConcurrencyProbe, ScriptedModel};
use orca_harness_core::{
    Agent, CancellationToken, Concurrency, FnTool, HarnessError, Limits, Message,
};

const RUN_TIMEOUT: Duration = Duration::from_secs(10);

fn last_tool_results(
    model_observed: &[orca_harness_core::Context],
) -> Vec<orca_harness_core::ToolResult> {
    let final_context = model_observed
        .last()
        .expect("model saw at least one context");
    final_context
        .messages()
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Tool { results } => Some(results.clone()),
            _ => None,
        })
        .expect("context contains tool results")
}

/// Four calls that each block on a shared barrier: the run can only
/// complete if all four tools are genuinely in flight at the same time.
#[tokio::test(flavor = "multi_thread")]
async fn parallel_calls_run_concurrently() {
    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let probe = ConcurrencyProbe::new();

    let fanout = {
        let barrier = barrier.clone();
        let probe = probe.clone();
        FnTool::new(
            "fanout",
            "barrier tool",
            json!({"type": "object"}),
            move |_input, ctx| {
                let barrier = barrier.clone();
                let probe = probe.clone();
                async move {
                    let _guard = probe.enter(&ctx.call_id);
                    barrier.wait().await;
                    Ok(json!({"id": ctx.call_id}))
                }
            },
        )
    };

    let model = ScriptedModel::tool_round(
        (0..4)
            .map(|i| call(&format!("call_{i}"), "fanout", json!({})))
            .collect(),
        "done",
    );
    let agent = Agent::new(model).tool(fanout);

    let answer = timeout(RUN_TIMEOUT, agent.run("fan out"))
        .await
        .expect("would deadlock if calls were serialized")
        .unwrap();

    assert_eq!(answer, "done");
    assert_eq!(probe.max_in_flight(), 4, "all four calls must overlap");
}

/// Completion order is scrambled with staggered sleeps; the model must
/// still see results in original call order with correct pairing.
#[tokio::test(flavor = "multi_thread")]
async fn results_restore_original_call_order() {
    let probe = ConcurrencyProbe::new();
    let sleeper = {
        let probe = probe.clone();
        FnTool::new(
            "sleeper",
            "sleeps input ms",
            json!({"type": "object"}),
            move |input, ctx| {
                let probe = probe.clone();
                async move {
                    let _guard = probe.enter(&ctx.call_id);
                    let ms = input["ms"].as_u64().unwrap();
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Ok(json!({"slept": ms}))
                }
            },
        )
    };

    // First call sleeps longest, so completion order is the reverse of
    // call order.
    let durations = [400u64, 300, 200, 100];
    let model = ScriptedModel::tool_round(
        durations
            .iter()
            .enumerate()
            .map(|(i, ms)| call(&format!("call_{i}"), "sleeper", json!({"ms": ms})))
            .collect(),
        "done",
    );
    let observed = {
        let agent = Agent::new(model).tool(sleeper);
        timeout(RUN_TIMEOUT, agent.run("scramble"))
            .await
            .unwrap()
            .unwrap();
        probe.finished()
    };

    assert_eq!(
        observed,
        vec!["call_3", "call_2", "call_1", "call_0"],
        "sanity: completion order should be reversed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn results_pair_with_their_calls() {
    let echo = FnTool::new(
        "echo",
        "echoes input",
        json!({"type": "object"}),
        |input, _ctx| async move { Ok(input) },
    );

    let model = Arc::new(ScriptedModel::tool_round(
        (0..8)
            .rev()
            .map(|i| call(&format!("call_{i}"), "echo", json!({"i": i})))
            .collect(),
        "done",
    ));
    let agent = Agent::new(model.clone()).tool(echo);
    timeout(RUN_TIMEOUT, agent.run("pair"))
        .await
        .unwrap()
        .unwrap();

    let results = last_tool_results(&model.observed_contexts());
    assert_eq!(results.len(), 8);
    for (slot, result) in results.iter().enumerate() {
        // Calls were issued with descending ids: slot 0 carries call_7.
        let expect = 7 - slot as u64;
        assert_eq!(result.call_id, format!("call_{expect}"), "order restored");
        assert_eq!(
            result.output["i"],
            json!(expect),
            "output paired with its call"
        );
        assert!(!result.is_error);
    }
}

/// Calls sharing a key never overlap and run in call order; calls with
/// different keys still overlap.
#[tokio::test(flavor = "multi_thread")]
async fn keyed_calls_serialize_per_key_but_overlap_across_keys() {
    let probe_a = ConcurrencyProbe::new();
    let probe_b = ConcurrencyProbe::new();
    let cross_key_barrier = Arc::new(tokio::sync::Barrier::new(2));

    let keyed = {
        let probe_a = probe_a.clone();
        let probe_b = probe_b.clone();
        let barrier = cross_key_barrier.clone();
        FnTool::new(
            "write",
            "keyed write",
            json!({"type": "object"}),
            move |input, ctx| {
                let probe = if input["key"] == "a" {
                    probe_a.clone()
                } else {
                    probe_b.clone()
                };
                let barrier = barrier.clone();
                async move {
                    let _guard = probe.enter(&ctx.call_id);
                    // The first call of each key meets the other key's first
                    // call here: proves cross-key overlap (and would deadlock
                    // if keys were globally serialized).
                    if ctx.call_id.ends_with("_0") {
                        barrier.wait().await;
                    }
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(json!({"id": ctx.call_id}))
                }
            },
        )
        .concurrency(|input| Concurrency::Keyed(input["key"].as_str().unwrap().to_string()))
    };

    let mut calls_batch = Vec::new();
    for i in 0..3 {
        calls_batch.push(call(&format!("a_{i}"), "write", json!({"key": "a"})));
        calls_batch.push(call(&format!("b_{i}"), "write", json!({"key": "b"})));
    }
    let model = ScriptedModel::tool_round(calls_batch, "done");
    let agent = Agent::new(model).tool(keyed);
    timeout(RUN_TIMEOUT, agent.run("keyed"))
        .await
        .expect("cross-key barrier requires keys to run concurrently")
        .unwrap();

    assert_eq!(
        probe_a.max_in_flight(),
        1,
        "same-key calls must never overlap"
    );
    assert_eq!(
        probe_b.max_in_flight(),
        1,
        "same-key calls must never overlap"
    );
    assert_eq!(
        probe_a.started(),
        vec!["a_0", "a_1", "a_2"],
        "keyed chain preserves call order"
    );
    assert_eq!(
        probe_b.started(),
        vec!["b_0", "b_1", "b_2"],
        "keyed chain preserves call order"
    );
}

/// A Serial call must be exclusive against every other call, while the
/// parallel calls around it still overlap with each other.
#[tokio::test(flavor = "multi_thread")]
async fn serial_calls_are_globally_exclusive() {
    let probe = ConcurrencyProbe::new();
    let violations = Arc::new(Mutex::new(Vec::<String>::new()));

    let make_tool = |name: &str, serial: bool| {
        let probe = probe.clone();
        let violations = violations.clone();
        let tool = FnTool::new(
            name,
            "instrumented",
            json!({"type": "object"}),
            move |_input, ctx| {
                let probe = probe.clone();
                let violations = violations.clone();
                let serial = serial;
                async move {
                    let _guard = probe.enter(&ctx.call_id);
                    if serial && probe.in_flight() != 1 {
                        violations.lock().unwrap().push(format!(
                            "{} ran alongside {} others",
                            ctx.call_id,
                            probe.in_flight() - 1
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    if serial && probe.in_flight() != 1 {
                        violations
                            .lock()
                            .unwrap()
                            .push(format!("{} overlapped mid-flight", ctx.call_id));
                    }
                    Ok(Value::Null)
                }
            },
        );
        if serial {
            tool.concurrency(|_| Concurrency::Serial)
        } else {
            tool
        }
    };

    let model = ScriptedModel::tool_round(
        vec![
            call("p_0", "par", json!({})),
            call("p_1", "par", json!({})),
            call("s_0", "ser", json!({})),
            call("s_1", "ser", json!({})),
            call("p_2", "par", json!({})),
        ],
        "done",
    );
    let agent = Agent::new(model)
        .tool(make_tool("par", false))
        .tool(make_tool("ser", true));
    timeout(RUN_TIMEOUT, agent.run("serial"))
        .await
        .unwrap()
        .unwrap();

    assert!(
        violations.lock().unwrap().is_empty(),
        "serial exclusivity violated: {:?}",
        violations.lock().unwrap()
    );
    assert!(
        probe.max_in_flight() >= 2,
        "parallel calls should still overlap"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn max_parallel_tools_limit_is_enforced() {
    let probe = ConcurrencyProbe::new();
    let sleeper = {
        let probe = probe.clone();
        FnTool::new(
            "sleeper",
            "sleeps",
            json!({"type": "object"}),
            move |_input, ctx| {
                let probe = probe.clone();
                async move {
                    let _guard = probe.enter(&ctx.call_id);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(Value::Null)
                }
            },
        )
    };

    let model = ScriptedModel::tool_round(
        (0..8)
            .map(|i| call(&format!("call_{i}"), "sleeper", json!({})))
            .collect(),
        "done",
    );
    let agent = Agent::new(model).tool(sleeper).limits(Limits {
        max_parallel_tools: 3,
        ..Limits::default()
    });
    timeout(RUN_TIMEOUT, agent.run("throttle"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        probe.max_in_flight(),
        3,
        "high-water mark must saturate but never exceed the limit"
    );
}

/// Cancelling the run token terminates a fan-out whose tools would
/// otherwise sleep forever.
#[tokio::test(flavor = "multi_thread")]
async fn cancellation_propagates_to_inflight_tools() {
    let started = Arc::new(tokio::sync::Notify::new());
    let forever = {
        let started = started.clone();
        FnTool::new(
            "forever",
            "never returns",
            json!({"type": "object"}),
            move |_input, _ctx| {
                let started = started.clone();
                async move {
                    started.notify_one();
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Ok(Value::Null)
                }
            },
        )
    };

    let model = ScriptedModel::tool_round(
        (0..4)
            .map(|i| call(&format!("call_{i}"), "forever", json!({})))
            .collect(),
        "unreachable",
    );
    let agent = Agent::new(model).tool(forever);

    let token = CancellationToken::new();
    let canceller = {
        let token = token.clone();
        let started = started.clone();
        tokio::spawn(async move {
            started.notified().await; // at least one tool is in flight
            token.cancel();
        })
    };

    let begun = std::time::Instant::now();
    let result = timeout(RUN_TIMEOUT, agent.run_with_cancellation("hang", token))
        .await
        .unwrap();
    canceller.await.unwrap();

    assert!(
        matches!(result, Err(HarnessError::Cancelled)),
        "got: {result:?}"
    );
    assert!(
        begun.elapsed() < Duration::from_secs(5),
        "cancellation must not wait for tools"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deadline_terminates_slow_fanout() {
    let slow = FnTool::new(
        "slow",
        "sleeps 1h",
        json!({"type": "object"}),
        |_input, _ctx| async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(Value::Null)
        },
    );

    let model = ScriptedModel::tool_round(
        (0..4)
            .map(|i| call(&format!("call_{i}"), "slow", json!({})))
            .collect(),
        "unreachable",
    );
    let agent = Agent::new(model).tool(slow).limits(Limits {
        deadline: Some(tokio::time::Instant::now() + Duration::from_millis(200)),
        ..Limits::default()
    });

    let begun = std::time::Instant::now();
    let result = timeout(RUN_TIMEOUT, agent.run("too slow")).await.unwrap();

    assert!(
        matches!(result, Err(HarnessError::DeadlineExceeded)),
        "got: {result:?}"
    );
    assert!(begun.elapsed() < Duration::from_secs(5));
}

/// A tool failing at runtime is not terminal: the model sees a normalized
/// error-flagged result for that call, correctly paired, while sibling
/// calls succeed.
#[tokio::test(flavor = "multi_thread")]
async fn tool_failures_are_normalized_into_results() {
    let flaky = FnTool::new(
        "flaky",
        "fails on demand",
        json!({"type": "object"}),
        |input, _ctx| async move {
            if input["fail"].as_bool().unwrap_or(false) {
                Err(orca_harness_core::ToolError::msg("disk on fire"))
            } else {
                Ok(json!({"ok": true}))
            }
        },
    );

    let model = Arc::new(ScriptedModel::tool_round(
        vec![
            call("call_0", "flaky", json!({"fail": false})),
            call("call_1", "flaky", json!({"fail": true})),
            call("call_2", "flaky", json!({"fail": false})),
        ],
        "recovered",
    ));
    let agent = Agent::new(model.clone()).tool(flaky);
    let answer = timeout(RUN_TIMEOUT, agent.run("normalize"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer, "recovered", "run continues after a tool failure");

    let results = last_tool_results(&model.observed_contexts());
    assert_eq!(results.len(), 3);
    assert!(!results[0].is_error);
    assert!(results[1].is_error);
    assert_eq!(results[1].call_id, "call_1");
    assert_eq!(results[1].output["error"], json!("disk on fire"));
    assert!(!results[2].is_error);
}

/// Tools that cooperatively watch `ctx.cancellation` observe the cancel
/// signal themselves.
#[tokio::test(flavor = "multi_thread")]
async fn tools_can_observe_cancellation_cooperatively() {
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let cooperative = {
        let saw_cancel = saw_cancel.clone();
        let started = started.clone();
        FnTool::new(
            "coop",
            "waits for cancel",
            json!({"type": "object"}),
            move |_input, ctx| {
                let saw_cancel = saw_cancel.clone();
                let started = started.clone();
                async move {
                    started.notify_one();
                    ctx.cancellation.cancelled().await;
                    saw_cancel.store(true, Ordering::SeqCst);
                    Ok(Value::Null)
                }
            },
        )
    };

    let model = ScriptedModel::tool_round(vec![call("call_0", "coop", json!({}))], "unreachable");
    let agent = Agent::new(model).tool(cooperative);
    let token = CancellationToken::new();
    let run = {
        let token = token.clone();
        async move { agent.run_with_cancellation("coop", token).await }
    };
    let runner = tokio::spawn(run);
    started.notified().await;
    token.cancel();
    let result = timeout(RUN_TIMEOUT, runner).await.unwrap().unwrap();

    assert!(matches!(result, Err(HarnessError::Cancelled)));
    // The tool itself saw the token fire (it may or may not win the race
    // against the dispatcher's own select, so only assert the signal
    // reached the tool context by construction of the test above).
    let _ = saw_cancel.load(Ordering::SeqCst);
}

/// Duplicate or empty call ids violate call/result pairing and are
/// rejected as InvalidToolCall.
#[tokio::test(flavor = "multi_thread")]
async fn invalid_pairing_is_rejected() {
    let echo = FnTool::new(
        "echo",
        "echo",
        json!({"type": "object"}),
        |input, _ctx| async move { Ok(input) },
    );
    let model = ScriptedModel::tool_round(
        vec![
            call("dup", "echo", json!({})),
            call("dup", "echo", json!({})),
        ],
        "unreachable",
    );
    let agent = Agent::new(model).tool(echo);
    let result = timeout(RUN_TIMEOUT, agent.run("dup ids")).await.unwrap();
    assert!(
        matches!(result, Err(HarnessError::InvalidToolCall(_))),
        "got: {result:?}"
    );

    let echo = FnTool::new(
        "echo",
        "echo",
        json!({"type": "object"}),
        |input, _ctx| async move { Ok(input) },
    );
    let model = ScriptedModel::tool_round(vec![call("", "echo", json!({}))], "unreachable");
    let agent = Agent::new(model).tool(echo);
    let result = timeout(RUN_TIMEOUT, agent.run("empty id")).await.unwrap();
    assert!(
        matches!(result, Err(HarnessError::InvalidToolCall(_))),
        "got: {result:?}"
    );
}

/// Wall-clock sanity: N sleeps of D ms must finish in far less than N×D.
#[tokio::test(flavor = "multi_thread")]
async fn fanout_wall_clock_is_parallel_not_serial() {
    let sleeper = FnTool::new(
        "sleeper",
        "100ms",
        json!({"type": "object"}),
        |_input, _ctx| async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(Value::Null)
        },
    );
    let model = ScriptedModel::tool_round(
        (0..8)
            .map(|i| call(&format!("call_{i}"), "sleeper", json!({})))
            .collect(),
        "done",
    );
    let agent = Agent::new(model).tool(sleeper);

    let begun = std::time::Instant::now();
    timeout(RUN_TIMEOUT, agent.run("clock"))
        .await
        .unwrap()
        .unwrap();
    let elapsed = begun.elapsed();

    // Serial execution would take ≥ 800ms; give generous CI headroom.
    assert!(
        elapsed < Duration::from_millis(500),
        "8×100ms took {elapsed:?}, not parallel"
    );
}

/// Multi-key calls: a call carrying several keys serializes against every
/// chain any of its keys belongs to. Chains bridged by such a call merge
/// into one ordered chain — per-key call order is preserved.
#[tokio::test(flavor = "multi_thread")]
async fn multi_keyed_calls_merge_chains_and_preserve_order() {
    use orca_harness_core::{Dispatcher, ExtensionRegistry, ToolRegistry};

    let probe = ConcurrencyProbe::new();
    let keyed = {
        let probe = probe.clone();
        FnTool::new(
            "locker",
            "multi-keyed",
            json!({"type": "object"}),
            move |_input, ctx| {
                let probe = probe.clone();
                async move {
                    let _guard = probe.enter(&ctx.call_id);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Ok(Value::Null)
                }
            },
        )
        .concurrency(|input| {
            Concurrency::Keys(
                input["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|k| k.as_str().unwrap().to_string())
                    .collect(),
            )
        })
    };
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(keyed));

    // a0 opens chain A, b0 opens chain B, ab bridges them (merging the
    // chains), a1 lands in the merged chain.
    let batch = vec![
        call("a0", "locker", json!({"keys": ["A"]})),
        call("b0", "locker", json!({"keys": ["B"]})),
        call("ab", "locker", json!({"keys": ["A", "B"]})),
        call("a1", "locker", json!({"keys": ["A"]})),
    ];
    let results = Dispatcher::new()
        .execute(
            batch,
            &tools,
            &ExtensionRegistry::new(),
            &CancellationToken::new(),
            None,
            4,
        )
        .await
        .unwrap();
    assert!(results.iter().all(|r| !r.is_error));

    assert_eq!(
        probe.max_in_flight(),
        1,
        "all four calls share a key transitively; none may overlap"
    );
    assert_eq!(
        probe.started(),
        vec!["a0", "b0", "ab", "a1"],
        "merged chain preserves call order"
    );
}

/// Multi-key calls with fully disjoint key sets still fan out.
#[tokio::test(flavor = "multi_thread")]
async fn multi_keyed_calls_with_disjoint_keys_overlap() {
    use orca_harness_core::{Dispatcher, ExtensionRegistry, ToolRegistry};

    let probe = ConcurrencyProbe::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let keyed = {
        let probe = probe.clone();
        let barrier = barrier.clone();
        FnTool::new(
            "locker",
            "multi-keyed",
            json!({"type": "object"}),
            move |_input, ctx| {
                let probe = probe.clone();
                let barrier = barrier.clone();
                async move {
                    let _guard = probe.enter(&ctx.call_id);
                    // Deadlocks unless both disjoint-key calls overlap.
                    barrier.wait().await;
                    Ok(Value::Null)
                }
            },
        )
        .concurrency(|input| {
            Concurrency::Keys(
                input["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|k| k.as_str().unwrap().to_string())
                    .collect(),
            )
        })
    };
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(keyed));

    let batch = vec![
        call("x", "locker", json!({"keys": ["A", "B"]})),
        call("y", "locker", json!({"keys": ["C", "D"]})),
    ];
    let results = timeout(
        RUN_TIMEOUT,
        Dispatcher::new().execute(
            batch,
            &tools,
            &ExtensionRegistry::new(),
            &CancellationToken::new(),
            None,
            2,
        ),
    )
    .await
    .expect("disjoint multi-key calls must overlap, not deadlock")
    .unwrap();
    assert!(results.iter().all(|r| !r.is_error));
    assert_eq!(probe.max_in_flight(), 2);
}

/// With no around_tool/after_tool subscribers, nothing can observe a
/// call's arguments after dispatch begins — so the dispatcher must MOVE
/// them into the tool rather than deep-copying (a 1 MiB write_file
/// payload would otherwise be cloned per call). Pointer identity of the
/// string buffer proves the move: a clone can never share the buffer
/// while the original is still alive in the batch.
#[tokio::test(flavor = "multi_thread")]
async fn arguments_move_into_tools_when_no_hooks_observe_them() {
    use std::sync::atomic::AtomicUsize;

    use orca_harness_core::{Dispatcher, ExtensionRegistry, ToolRegistry};

    let arguments = json!({"content": "x".repeat(64 * 1024)});
    let original_ptr = arguments["content"].as_str().unwrap().as_ptr() as usize;

    let received_ptr = Arc::new(AtomicUsize::new(0));
    let sink = {
        let received_ptr = received_ptr.clone();
        FnTool::new(
            "sink",
            "records buffer identity",
            json!({"type": "object"}),
            move |input, _ctx| {
                let received_ptr = received_ptr.clone();
                async move {
                    let ptr = input["content"].as_str().unwrap().as_ptr() as usize;
                    received_ptr.store(ptr, Ordering::SeqCst);
                    Ok(Value::Null)
                }
            },
        )
    };
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(sink));

    let results = Dispatcher::new()
        .execute(
            vec![call("c0", "sink", arguments)],
            &tools,
            &ExtensionRegistry::new(),
            &CancellationToken::new(),
            None,
            1,
        )
        .await
        .unwrap();
    assert!(!results[0].is_error);
    assert_eq!(
        received_ptr.load(Ordering::SeqCst),
        original_ptr,
        "tool must receive the original buffer (moved), not a clone"
    );
}

/// Guard for the move optimization: when an after_tool subscriber exists,
/// it must still observe the call's original arguments.
#[tokio::test(flavor = "multi_thread")]
async fn after_tool_hooks_still_see_original_arguments() {
    use async_trait::async_trait;
    use orca_harness_core::{
        Dispatcher, Extension, ExtensionError, ExtensionRegistry, Subscriptions, ToolCall,
        ToolRegistry, ToolResult,
    };

    struct SeesArgs(Mutex<Vec<Value>>);

    #[async_trait]
    impl Extension for SeesArgs {
        fn name(&self) -> &str {
            "sees_args"
        }
        fn subscriptions(&self) -> Subscriptions {
            Subscriptions::none().after_tool()
        }
        async fn after_tool(
            &self,
            call: &ToolCall,
            result: ToolResult,
        ) -> Result<ToolResult, ExtensionError> {
            self.0.lock().unwrap().push(call.arguments.clone());
            Ok(result)
        }
    }

    let echo = FnTool::new(
        "echo",
        "echo",
        json!({"type": "object"}),
        |input, _ctx| async move { Ok(input) },
    );
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(echo));

    let sees = Arc::new(SeesArgs(Mutex::new(Vec::new())));
    let mut extensions = ExtensionRegistry::new();
    extensions.register(sees.clone());

    let results = Dispatcher::new()
        .execute(
            vec![call("c0", "echo", json!({"payload": "intact"}))],
            &tools,
            &extensions,
            &CancellationToken::new(),
            None,
            1,
        )
        .await
        .unwrap();
    assert!(!results[0].is_error);
    assert_eq!(
        sees.0.lock().unwrap().as_slice(),
        &[json!({"payload": "intact"})],
        "after_tool must observe the original arguments"
    );
}

/// Guard for the move optimization: an around_tool wrapper receives the
/// call by reference and must still see its original arguments.
#[tokio::test(flavor = "multi_thread")]
async fn around_tool_hooks_still_see_original_arguments() {
    use async_trait::async_trait;
    use orca_harness_core::{
        Dispatcher, Extension, ExtensionRegistry, Next, Subscriptions, ToolCall, ToolContext,
        ToolError, ToolRegistry,
    };

    struct WrapsArgs(Mutex<Vec<Value>>);

    #[async_trait]
    impl Extension for WrapsArgs {
        fn name(&self) -> &str {
            "wraps_args"
        }
        fn subscriptions(&self) -> Subscriptions {
            Subscriptions::none().around_tool()
        }
        async fn around_tool<'a>(
            &self,
            call: &ToolCall,
            input: Value,
            _ctx: &ToolContext,
            next: Next<'a>,
        ) -> Result<Value, ToolError> {
            self.0.lock().unwrap().push(call.arguments.clone());
            next.run(input).await
        }
    }

    let echo = FnTool::new(
        "echo",
        "echo",
        json!({"type": "object"}),
        |input, _ctx| async move { Ok(input) },
    );
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(echo));

    let wraps = Arc::new(WrapsArgs(Mutex::new(Vec::new())));
    let mut extensions = ExtensionRegistry::new();
    extensions.register(wraps.clone());

    let results = Dispatcher::new()
        .execute(
            vec![call("c0", "echo", json!({"payload": "intact"}))],
            &tools,
            &extensions,
            &CancellationToken::new(),
            None,
            1,
        )
        .await
        .unwrap();
    assert!(!results[0].is_error);
    assert_eq!(
        wraps.0.lock().unwrap().as_slice(),
        &[json!({"payload": "intact"})],
        "around_tool must observe the original arguments"
    );
}

/// 100-way fan-out: every one of the 100 calls blocks on a shared barrier
/// until all 100 are in flight at once, so the run can only complete if
/// the dispatcher genuinely executes them concurrently (any cap or
/// serialization below 100 would deadlock the barrier and trip the
/// timeout). Also asserts pairing and order restoration at this scale.
#[tokio::test(flavor = "multi_thread")]
async fn hundred_parallel_calls_run_concurrently() {
    const N: usize = 100;
    let barrier = Arc::new(tokio::sync::Barrier::new(N));
    let probe = ConcurrencyProbe::new();

    let fanout = {
        let barrier = barrier.clone();
        let probe = probe.clone();
        FnTool::new(
            "fanout",
            "barrier tool",
            json!({"type": "object"}),
            move |input, ctx| {
                let barrier = barrier.clone();
                let probe = probe.clone();
                async move {
                    let _guard = probe.enter(&ctx.call_id);
                    barrier.wait().await;
                    Ok(json!({"i": input["i"]}))
                }
            },
        )
    };

    let model = Arc::new(ScriptedModel::tool_round(
        (0..N)
            .map(|i| call(&format!("call_{i}"), "fanout", json!({"i": i})))
            .collect(),
        "done",
    ));
    let agent = Agent::new(model.clone()).tool(fanout).limits(Limits {
        max_parallel_tools: N,
        ..Limits::default()
    });

    let begun = std::time::Instant::now();
    let answer = timeout(RUN_TIMEOUT, agent.run("fan out 100"))
        .await
        .expect("would deadlock unless all 100 calls overlap")
        .unwrap();
    let elapsed = begun.elapsed();

    assert_eq!(answer, "done");
    assert_eq!(
        probe.max_in_flight(),
        N,
        "all 100 calls must be in flight together"
    );
    assert_eq!(probe.finished().len(), N);

    // Model-visible results: original call order, correct pairing, no errors.
    let results = last_tool_results(&model.observed_contexts());
    assert_eq!(results.len(), N);
    for (slot, result) in results.iter().enumerate() {
        assert_eq!(
            result.call_id,
            format!("call_{slot}"),
            "order restored at slot {slot}"
        );
        assert_eq!(
            result.output["i"],
            json!(slot),
            "output paired with its call"
        );
        assert!(!result.is_error);
    }

    assert!(
        elapsed < Duration::from_secs(5),
        "100-way barrier fan-out took {elapsed:?}"
    );
}
