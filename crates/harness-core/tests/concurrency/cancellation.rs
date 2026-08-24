/// An interruption mid-batch must not discard completed sibling results:
/// the finished call keeps its real output; only the interrupted call
/// carries an error saying what stopped it. (Previously one cancelled
/// call failed the whole dispatch and every result was dropped.)
#[tokio::test(flavor = "multi_thread")]
async fn cancellation_preserves_completed_sibling_results() {
    use orca_harness_core::{Dispatcher, ExtensionRegistry, ToolRegistry};

    let token = CancellationToken::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    let fast = {
        let barrier = barrier.clone();
        FnTool::new(
            "fast",
            "returns once both are in flight",
            json!({"type": "object"}),
            move |_input, _ctx| {
                let barrier = barrier.clone();
                async move {
                    barrier.wait().await;
                    Ok(json!({"ok": true}))
                }
            },
        )
    };
    let hang = {
        let barrier = barrier.clone();
        let token = token.clone();
        FnTool::new(
            "hang",
            "cancels the run, then never returns",
            json!({"type": "object"}),
            move |_input, _ctx| {
                let barrier = barrier.clone();
                let token = token.clone();
                async move {
                    barrier.wait().await;
                    // Let the fast sibling finish well before the cancel.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    token.cancel();
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Ok(Value::Null)
                }
            },
        )
    };
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(fast));
    tools.register(Arc::new(hang));

    let results = timeout(
        RUN_TIMEOUT,
        Dispatcher::new().execute(
            vec![
                call("c_fast", "fast", json!({})),
                call("c_hang", "hang", json!({})),
            ],
            &tools,
            &ExtensionRegistry::new(),
            &token,
            None,
            4,
        ),
    )
    .await
    .expect("cancellation must resolve the batch, not hang it")
    .unwrap();

    assert_eq!(results.len(), 2);
    assert!(
        !results[0].is_error,
        "completed call keeps its result: {:?}",
        results[0]
    );
    assert_eq!(results[0].output, json!({"ok": true}));
    assert!(results[1].is_error);
    assert!(
        results[1].output["error"]
            .as_str()
            .unwrap()
            .contains("cancelled"),
        "interrupted call must say so: {:?}",
        results[1]
    );
}

/// Same batch-honesty guarantee for the run deadline: the call that
/// finished under the deadline keeps its output, the one that blew it
/// gets a deadline error.
#[tokio::test(flavor = "multi_thread")]
async fn deadline_preserves_completed_sibling_results() {
    use orca_harness_core::{Dispatcher, ExtensionRegistry, ToolRegistry};

    let fast = FnTool::new(
        "fast",
        "instant",
        json!({"type": "object"}),
        |_input, _ctx| async move { Ok(json!({"ok": true})) },
    );
    let slow = FnTool::new(
        "slow",
        "sleeps 1h",
        json!({"type": "object"}),
        |_input, _ctx| async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(Value::Null)
        },
    );
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(fast));
    tools.register(Arc::new(slow));

    let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
    let results = timeout(
        RUN_TIMEOUT,
        Dispatcher::new().execute(
            vec![
                call("c_fast", "fast", json!({})),
                call("c_slow", "slow", json!({})),
            ],
            &tools,
            &ExtensionRegistry::new(),
            &CancellationToken::new(),
            Some(deadline),
            4,
        ),
    )
    .await
    .unwrap()
    .unwrap();

    assert!(!results[0].is_error, "got: {:?}", results[0]);
    assert_eq!(results[0].output, json!({"ok": true}));
    assert!(results[1].is_error);
    assert!(
        results[1].output["error"]
            .as_str()
            .unwrap()
            .contains("deadline"),
        "got: {:?}",
        results[1]
    );
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
