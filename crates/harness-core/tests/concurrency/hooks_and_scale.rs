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
