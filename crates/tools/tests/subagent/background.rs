struct GateModel {
    release: CancellationToken,
}

#[async_trait]
impl Model for GateModel {
    async fn generate(
        &self,
        _context: &Context,
        _tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.release.cancelled().await;
        Ok(ModelResponse::final_text("background complete"))
    }
}

#[tokio::test]
async fn background_is_default_and_notifies_after_parent_cancellation() {
    let release = CancellationToken::new();
    let model = Arc::new(GateModel {
        release: release.clone(),
    });
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(1);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let stats = orca_harness_tools::BackgroundStats::new();
    let tool = SubagentTool::new(model, &ws)
        .stats(stats.clone())
        .background(manager.clone(), move |notification| {
            let _ = tx.send(notification);
        });

    assert_eq!(
        tool.schema().parameters["properties"]["background"]["default"],
        true
    );
    let parent_cancel = CancellationToken::new();
    let tctx = ToolContext {
        call_id: "outer-background".into(),
        tool_name: "subagent".into(),
        cancellation: parent_cancel.clone(),
        deadline: None,
    };
    let acknowledged = tokio::time::timeout(
        Duration::from_millis(100),
        tool.call(json!({"task": "work independently"}), &tctx),
    )
    .await
    .expect("background admission must not wait for the child")
    .unwrap();

    assert_eq!(acknowledged["status"], "running");
    assert_eq!(acknowledged["termination"], "detached");
    assert_eq!(stats.agents(), 1);
    assert!(rx.try_recv().is_err());

    // A detached worker belongs to the session manager, not the parent turn.
    parent_cancel.cancel();
    release.cancel();
    let completion = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("completion notification timed out")
        .expect("completion channel closed");
    let generation = completion.generation;
    assert!(manager.is_current(generation));
    assert_eq!(completion.spawn.call_id, "outer-background");
    assert_eq!(completion.result.unwrap()["answer"], "background complete");
    assert_eq!(stats.agents(), 0);
    manager.cancel_all();
    assert!(!manager.is_current(generation));
}

#[tokio::test]
async fn explicit_foreground_returns_the_answer_without_background_delivery() {
    let model = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text(
        "direct answer",
    )]));
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(1);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = SubagentTool::new(model, &ws).background(manager.clone(), move |notification| {
        let _ = tx.send(notification);
    });
    let result = tool
        .call(
            json!({"task": "answer directly", "background": false}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(result["answer"], "direct answer");
    assert_eq!(result["termination"], "completed");
    assert!(manager.active().is_empty());
    assert!(rx.try_recv().is_err());
}

/// Counts how many inner agents actually reached the model, then holds
/// them until released.
struct CountingGateModel {
    started: Arc<std::sync::atomic::AtomicUsize>,
    release: CancellationToken,
}

#[async_trait]
impl Model for CountingGateModel {
    async fn generate(
        &self,
        _context: &Context,
        _tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.release.cancelled().await;
        Ok(ModelResponse::final_text("done"))
    }
}

#[tokio::test]
async fn background_subagents_beyond_the_limit_queue_in_spawn_order() {
    let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let release = CancellationToken::new();
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(2);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = SubagentTool::new(
        Arc::new(CountingGateModel {
            started: started.clone(),
            release: release.clone(),
        }),
        &ws,
    )
    .background(manager.clone(), move |notification| {
        let _ = tx.send(notification);
    });

    let mut acks = Vec::new();
    for i in 0..3 {
        acks.push(
            tool.call(
                json!({"task": format!("job {i}"), "background": true}),
                &ctx(),
            )
            .await
            .unwrap(),
        );
    }
    // The acknowledgement says which spawns hold a slot and which wait.
    assert_eq!(acks[0]["status"], "running");
    assert_eq!(acks[1]["status"], "running");
    assert_eq!(acks[2]["status"], "queued");
    let listed = tool.call(json!({"action": "list"}), &ctx()).await.unwrap();
    assert_eq!(listed["count"], 3);
    assert_eq!(listed["agents"][1]["status"], "running");
    assert_eq!(listed["agents"][2]["status"], "queued");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 2);

    release.cancel();
    for _ in 0..3 {
        tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("completion notification timed out")
            .expect("completion channel closed");
    }
    assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(manager.active().len(), 0);
}

#[tokio::test]
async fn raising_the_background_limit_admits_queued_subagents_immediately() {
    let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let release = CancellationToken::new();
    let (ws, _dir) = temp_ws();
    let settings = orca_harness_tools::SubagentDepth::default();
    settings.set_background_limit(1);
    let manager = orca_harness_tools::SubagentManager::from_settings(settings.clone());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = SubagentTool::new(
        Arc::new(CountingGateModel {
            started: started.clone(),
            release: release.clone(),
        }),
        &ws,
    )
    .background(manager.clone(), move |notification| {
        let _ = tx.send(notification);
    });

    for i in 0..3 {
        tool.call(
            json!({"task": format!("job {i}"), "background": true}),
            &ctx(),
        )
        .await
        .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 1);

    settings.set_background_limit(0);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        started.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "removing the limit must start queued jobs without waiting for a release"
    );
    assert!(manager
        .active()
        .iter()
        .all(|job| job.status == orca_harness_tools::BackgroundStatus::Running));

    release.cancel();
    for _ in 0..3 {
        tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("completion notification timed out")
            .expect("completion channel closed");
    }
}

#[tokio::test]
async fn cancelling_a_queued_subagent_frees_its_place_in_line() {
    let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let release = CancellationToken::new();
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(1);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = SubagentTool::new(
        Arc::new(CountingGateModel {
            started: started.clone(),
            release: release.clone(),
        }),
        &ws,
    )
    .background(manager.clone(), move |notification| {
        let _ = tx.send(notification);
    });

    let mut ids = Vec::new();
    for i in 0..3 {
        let ack = tool
            .call(
                json!({"task": format!("job {i}"), "background": true}),
                &ctx(),
            )
            .await
            .unwrap();
        ids.push(ack["spawnId"].as_u64().unwrap());
    }
    // Cancel the head of the queue, then release the running job: the
    // third spawn must start rather than wait behind a cancelled one.
    assert!(manager.cancel(ids[1]));
    let cancelled = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("cancelled job did not report")
        .expect("completion channel closed");
    assert_eq!(cancelled.spawn.id, ids[1]);
    assert!(cancelled.result.is_err());
    assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 1);

    release.cancel();
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("completion notification timed out")
            .expect("completion channel closed");
    }
    assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn background_spawn_ids_remain_unique_when_the_host_rebuilds_the_tool() {
    let release = CancellationToken::new();
    let manager = orca_harness_tools::SubagentManager::new(2);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (ws, _dir) = temp_ws();
    let make_tool = || {
        let tx = tx.clone();
        SubagentTool::new(
            Arc::new(GateModel {
                release: release.clone(),
            }),
            &ws,
        )
        .background(manager.clone(), move |notification| {
            let _ = tx.send(notification);
        })
    };

    let first = make_tool()
        .call(json!({"task": "first", "background": true}), &ctx())
        .await
        .unwrap();
    let second = make_tool()
        .call(json!({"task": "second", "background": true}), &ctx())
        .await
        .unwrap();

    assert_eq!(first["spawnId"], 0);
    assert_eq!(second["spawnId"], 1);
    release.cancel();
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("completion notification timed out")
            .expect("completion channel closed");
    }
}

#[tokio::test]
async fn parent_can_list_cancel_and_cancel_all_background_subagents() {
    let release = CancellationToken::new();
    let model = Arc::new(GateModel {
        release: release.clone(),
    });
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(3);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = SubagentTool::new(model, &ws).background(manager, move |notification| {
        let _ = tx.send(notification);
    });

    let schema = tool.schema();
    assert!(schema.parameters["properties"]["action"].is_object());
    assert!(schema
        .description
        .contains("call this subagent tool, not process"));
    let first = tool
        .call(json!({"task": "first", "background": true}), &ctx())
        .await
        .unwrap();
    let first_id = first["spawnId"].as_u64().unwrap();
    let listed = tool.call(json!({"action": "list"}), &ctx()).await.unwrap();
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["agents"][0]["spawnId"], first_id);
    assert_eq!(listed["agents"][0]["task"], "first");

    let cancelled = tool
        .call(json!({"action": "cancel", "spawnId": first_id}), &ctx())
        .await
        .unwrap();
    assert_eq!(cancelled["cancelled"], true);
    let completion = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("cancelled child did not finish")
        .expect("completion channel closed");
    assert!(completion.result.is_err());
    assert_eq!(
        tool.call(json!({"action": "list"}), &ctx()).await.unwrap()["count"],
        0
    );

    for task in ["second", "third"] {
        tool.call(json!({"task": task, "background": true}), &ctx())
            .await
            .unwrap();
    }
    let stopped = tool
        .call(json!({"action": "cancel_all"}), &ctx())
        .await
        .unwrap();
    assert_eq!(stopped["cancelled"], 2);
    assert_eq!(
        tool.call(json!({"action": "list"}), &ctx()).await.unwrap()["count"],
        0
    );
}

#[tokio::test]
async fn background_input_is_rejected_when_the_host_did_not_enable_it() {
    let model = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text(
        "unused",
    )]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model, &ws);

    assert!(tool.schema().parameters["properties"]["background"].is_null());
    let error = tool
        .call(json!({"task": "no host", "background": true}), &ctx())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("background subagents are unavailable"));
}

/// Legacy waits must return while both running and queued children remain held.
#[tokio::test]
async fn legacy_wait_is_rejected_without_blocking_background_workers() {
    let release = CancellationToken::new();
    let model = Arc::new(GateModel {
        release: release.clone(),
    });
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(1);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = SubagentTool::new(model, &ws).background(manager.clone(), move |notification| {
        let _ = tx.send(notification);
    });
    assert!(!tool.schema().parameters["properties"]["action"]["enum"]
        .as_array()
        .unwrap()
        .contains(&json!("wait")));
    assert!(tool.schema().description.contains("end your turn"));
    assert!(tool
        .schema()
        .description
        .contains("Cancel them only when the user explicitly asks"));
    for task in ["first", "second"] {
        tool.call(json!({"task": task, "background": true}), &ctx())
            .await
            .unwrap();
    }
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        tool.call(json!({"action": "wait"}), &ctx()),
    )
    .await
    .expect("legacy wait must not block")
    .unwrap_err()
    .to_string();
    assert!(error.contains("end your turn"), "{error}");
    assert_eq!(manager.active().len(), 2);
    assert!(rx.try_recv().is_err());
    release.cancel();
    for _ in 0..2 {
        assert!(tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .result
            .is_ok());
    }
}

/// Repeating `list` with nothing changed is polling; the second call is
/// refused with guidance to end the turn, and any state change lifts the refusal.
#[tokio::test]
async fn repeated_unchanged_list_is_refused() {
    let release = CancellationToken::new();
    let model = Arc::new(GateModel {
        release: release.clone(),
    });
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(1);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = SubagentTool::new(model, &ws).background(manager, move |notification| {
        let _ = tx.send(notification);
    });

    // Empty lists are never refused: there is nothing to poll for.
    tool.call(json!({"action": "list"}), &ctx()).await.unwrap();
    tool.call(json!({"action": "list"}), &ctx()).await.unwrap();

    tool.call(json!({"task": "held", "background": true}), &ctx())
        .await
        .unwrap();
    let listed = tool.call(json!({"action": "list"}), &ctx()).await.unwrap();
    assert_eq!(listed["count"], 1);
    let refused = tool
        .call(json!({"action": "list"}), &ctx())
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("no change since the previous list"), "{refused}");
    assert!(refused.contains("end your turn"), "{refused}");

    release.cancel();
    rx.recv().await.expect("worker completion");
    let listed = tool.call(json!({"action": "list"}), &ctx()).await.unwrap();
    assert_eq!(listed["count"], 0);
}
