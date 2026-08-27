#[tokio::test]
async fn spawn_extensions_receive_identity_and_events() {
    let spawns: Arc<Mutex<Vec<SubagentSpawn>>> = Arc::new(Mutex::new(Vec::new()));
    let events: Arc<Mutex<Vec<(u64, HarnessEvent)>>> = Arc::new(Mutex::new(Vec::new()));

    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("1", "list_dir", json!({"path": "."}))],
        "explored",
    ));
    let (ws, _dir) = temp_ws();
    let recorded = spawns.clone();
    let sink = events.clone();
    let tool = SubagentTool::new(model, &ws).spawn_extensions(Arc::new(move |spawn| {
        recorded.lock().unwrap().push(spawn.clone());
        let sink = sink.clone();
        let id = spawn.id;
        vec![Arc::new(EventStream::from_fn(move |event| {
            sink.lock().unwrap().push((id, event));
        })) as Arc<dyn Extension>]
    }));

    let tctx = ToolContext {
        call_id: "outer-call-7".into(),
        tool_name: "subagent".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    };
    tool.call(json!({"task": "explore"}), &tctx).await.unwrap();

    let spawns = spawns.lock().unwrap();
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].depth, 0);
    assert_eq!(spawns[0].parent_id, None);
    assert_eq!(spawns[0].call_id, "outer-call-7");
    assert_eq!(spawns[0].task, "explore");
    assert!(spawns[0].identity.is_none());

    let events = events.lock().unwrap();
    assert!(events.iter().any(|(id, e)| *id == spawns[0].id
        && matches!(e, HarnessEvent::ToolCall { tool_name, .. } if tool_name == "list_dir")));
    assert!(events
        .iter()
        .any(|(id, e)| *id == spawns[0].id && matches!(e, HarnessEvent::Result { .. })));
}

#[tokio::test]
async fn nested_spawns_link_parent_and_depth() {
    let spawns: Arc<Mutex<Vec<SubagentSpawn>>> = Arc::new(Mutex::new(Vec::new()));
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call("1", "subagent", json!({"task": "inner"}))]),
        ModelResponse::final_text("grandchild done"),
        ModelResponse::final_text("child done"),
    ]));
    let (ws, _dir) = temp_ws();
    let recorded = spawns.clone();
    let tool = SubagentTool::new(model, &ws)
        .max_depth(SubagentDepth::new(2))
        .spawn_extensions(Arc::new(move |spawn| {
            recorded.lock().unwrap().push(spawn.clone());
            Vec::new()
        }));
    tool.call(json!({"task": "outer"}), &ctx()).await.unwrap();

    let spawns = spawns.lock().unwrap();
    assert_eq!(spawns.len(), 2);
    assert_eq!(spawns[0].depth, 0);
    assert_eq!(spawns[1].depth, 1);
    assert_eq!(spawns[1].parent_id, Some(spawns[0].id));
    assert_ne!(spawns[0].id, spawns[1].id);
}

#[tokio::test]
async fn agent_count_rises_and_falls_even_on_cancel() {
    let stats = BackgroundStats::new();
    let (ws, _dir) = temp_ws();
    let tool = Arc::new(SubagentTool::new(Arc::new(StallModel), &ws).stats(stats.clone()));
    let cancel = CancellationToken::new();
    let tctx = ToolContext {
        call_id: "t".into(),
        tool_name: "subagent".into(),
        cancellation: cancel.clone(),
        deadline: None,
    };
    let running = tool.clone();
    let handle = tokio::spawn(async move { running.call(json!({"task": "stall"}), &tctx).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(stats.agents(), 1);
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap();
    assert_eq!(stats.agents(), 0);

    // Errors decrement too: a model with no script fails immediately.
    let empty = Arc::new(ScriptedModel::new(vec![]));
    let tool = SubagentTool::new(empty, &ws).stats(stats.clone());
    let _ = tool.call(json!({"task": "x"}), &ctx()).await;
    assert_eq!(stats.agents(), 0);
}
