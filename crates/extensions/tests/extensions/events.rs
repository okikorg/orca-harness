#[tokio::test]
async fn event_stream_emits_full_lifecycle() {
    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("thinking".into()),
            calls: vec![call("c0", "echo", json!({"v": 1}))],
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
        },
        ModelResponse::Final {
            text: "all done".into(),
            usage: Some(Usage {
                input_tokens: 3,
                output_tokens: 7,
                ..Default::default()
            }),
        },
    ]);

    let (stream, mut rx) = EventStream::channel();
    let agent = Agent::new(model).tool(echo()).extension(stream);
    timeout(RUN_TIMEOUT, agent.run("go"))
        .await
        .unwrap()
        .unwrap();

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    // Expect: AgentStart, Assistant(thinking), Usage, ToolCall, ToolFinished,
    // ToolResult, Assistant(all done), Usage, Result.
    let tags: Vec<&str> = events
        .iter()
        .map(|e| match e {
            HarnessEvent::AgentStart => "start",
            HarnessEvent::AssistantDelta { .. } => "assistant_delta",
            HarnessEvent::ReasoningDelta { .. } => "reasoning_delta",
            HarnessEvent::ToolInputDelta { .. } => "tool_input_delta",
            HarnessEvent::Assistant { .. } => "assistant",
            HarnessEvent::ToolCall { .. } => "tool_call",
            HarnessEvent::ToolFinished { .. } => "tool_finished",
            HarnessEvent::ToolResult { .. } => "tool_result",
            HarnessEvent::Usage { .. } => "usage",
            HarnessEvent::Result { .. } => "result",
            HarnessEvent::Error { .. } => "error",
        })
        .collect();
    assert_eq!(
        tags,
        vec![
            "start",
            "assistant",
            "usage",
            "tool_call",
            "tool_finished",
            "tool_result",
            "assistant",
            "usage",
            "result"
        ]
    );

    // Tool call/result carry the right identity.
    match &events[3] {
        HarnessEvent::ToolCall {
            tool_call_id,
            tool_name,
            input,
        } => {
            assert_eq!(tool_call_id, "c0");
            assert_eq!(tool_name, "echo");
            assert_eq!(input, &json!({"v": 1}));
        }
        other => panic!("expected tool_call, got {other:?}"),
    }
    match &events[5] {
        HarnessEvent::ToolResult {
            tool_call_id,
            output,
            is_error,
            ..
        } => {
            assert_eq!(tool_call_id, "c0");
            assert_eq!(output, &json!({"v": 1}));
            assert!(!is_error);
        }
        other => panic!("expected tool_result, got {other:?}"),
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn event_stream_emits_live_completion_before_slow_sibling_finishes() {
    let sleeper = FnTool::new(
        "sleeper",
        "sleeps for input milliseconds",
        json!({"type": "object"}),
        |input, _ctx| async move {
            let ms = input["ms"].as_u64().unwrap();
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(json!({"slept": ms}))
        },
    );
    let model = ScriptedModel::tool_round(
        vec![
            call("fast", "sleeper", json!({"ms": 10})),
            call("slow", "sleeper", json!({"ms": 500})),
        ],
        "done",
    );
    let (stream, mut rx) = EventStream::channel();
    let run = tokio::spawn(async move {
        Agent::new(model)
            .tool(sleeper)
            .extension(stream)
            .run("go")
            .await
    });

    timeout(Duration::from_millis(200), async {
        loop {
            if matches!(
                rx.recv().await,
                Some(HarnessEvent::ToolFinished { ref tool_call_id, .. }) if tool_call_id == "fast"
            ) {
                break;
            }
        }
    })
    .await
    .expect("fast completion must not wait for the slow sibling");
    assert!(!run.is_finished(), "slow sibling should still be executing");

    run.await.unwrap().unwrap();
    let mut result_ids = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let HarnessEvent::ToolResult { tool_call_id, .. } = event {
            result_ids.push(tool_call_id);
        }
    }
    assert_eq!(
        result_ids,
        ["fast", "slow"],
        "final results remain deterministic and call-ordered"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn event_stream_emits_completion_for_calls_cancelled_before_execution() {
    let slow = FnTool::new(
        "slow",
        "waits forever",
        json!({"type": "object"}),
        |_input, _ctx| async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(Value::Null)
        },
    );
    let model = ScriptedModel::tool_round(
        vec![
            call("waiting", "slow", json!({})),
            call("running", "slow", json!({})),
        ],
        "unreachable",
    );
    let (stream, mut rx) = EventStream::channel();
    let cancellation = CancellationToken::new();
    let run_token = cancellation.clone();
    let run = tokio::spawn(async move {
        Agent::new(model)
            .tool(slow)
            .extension(stream)
            .limits(Limits {
                max_parallel_tools: 1,
                ..Limits::default()
            })
            .run_with_cancellation("go", run_token)
            .await
    });

    tokio::time::sleep(Duration::from_millis(20)).await;
    cancellation.cancel();
    assert!(run.await.unwrap().is_err());

    let mut finished = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let HarnessEvent::ToolFinished {
            tool_call_id,
            is_error,
            ..
        } = event
        {
            finished.push((tool_call_id, is_error));
        }
    }
    finished.sort();
    assert_eq!(
        finished,
        [("running".into(), true), ("waiting".into(), true)]
    );
}

#[tokio::test]
async fn event_stream_serializes_to_platform_ndjson_tags() {
    // The tag names must match the platform NDJSON union.
    let ev = HarnessEvent::ToolFinished {
        tool_call_id: "c0".into(),
        tool_name: "echo".into(),
        is_error: false,
    };
    let s = serde_json::to_value(&ev).unwrap();
    assert_eq!(s["type"], json!("tool_finished"));
    assert_eq!(s["tool_call_id"], json!("c0"));

    let ev = HarnessEvent::ToolResult {
        tool_call_id: "c0".into(),
        tool_name: "echo".into(),
        output: json!({"ok": true}),
        is_error: false,
    };
    let s = serde_json::to_value(&ev).unwrap();
    assert_eq!(s["type"], json!("tool_result"));
    assert_eq!(s["tool_call_id"], json!("c0"));
}

#[tokio::test]
async fn event_stream_emits_error_on_failure() {
    let model = ScriptedModel::new(vec![]); // model error
    let (stream, mut rx) = EventStream::channel();
    let agent = Agent::new(model).extension(stream);
    let _ = timeout(RUN_TIMEOUT, agent.run("boom")).await.unwrap();

    let mut saw_error = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, HarnessEvent::Error { .. }) {
            saw_error = true;
        }
    }
    assert!(saw_error, "an error event must be emitted");
}
