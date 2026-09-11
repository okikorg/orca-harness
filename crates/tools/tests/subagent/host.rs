// Typed host operations must behave exactly like the model tool: same
// routing validation, admission, spawn ids, nesting, and cancellation.

use orca_harness_tools::{BackgroundStatus, SubagentRequest};

fn scripted_final(text: &str) -> Arc<ScriptedModel> {
    Arc::new(ScriptedModel::new(vec![
        ModelResponse::Final {
            text: text.into(),
            usage: Some(Usage {
                input_tokens: 11,
                output_tokens: 7,
                ..Usage::default()
            }),
        },
        ModelResponse::Final {
            text: text.into(),
            usage: Some(Usage {
                input_tokens: 11,
                output_tokens: 7,
                ..Usage::default()
            }),
        },
    ]))
}

#[tokio::test]
async fn typed_foreground_run_matches_the_tool_result() {
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(scripted_final("same answer"), &ws)
        .inherited_identity("test", "worker");

    let via_tool = tool
        .call(json!({"task": "compare"}), &ctx())
        .await
        .unwrap();
    let typed = tool
        .run_foreground(
            SubagentRequest::new("compare"),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert_eq!(typed.answer, "same answer");
    assert_eq!(typed.identity.as_ref().unwrap().model, "worker");
    let mut typed_json = typed.into_value();
    let mut tool_json = via_tool;
    // Wall-clock is the only field that legitimately differs.
    typed_json["runtimeMs"] = json!(0);
    tool_json["runtimeMs"] = json!(0);
    assert_eq!(typed_json, tool_json);
}

#[tokio::test]
async fn typed_background_spawn_shares_spawn_ids_and_notifies() {
    let release = CancellationToken::new();
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(2);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = SubagentTool::new(
        Arc::new(GateModel {
            release: release.clone(),
        }),
        &ws,
    )
    .background(manager.clone(), move |notification| {
        let _ = tx.send(notification);
    });
    assert!(tool.has_background());

    let via_tool = tool
        .call(json!({"task": "model job", "background": true}), &ctx())
        .await
        .unwrap();
    let typed = tool
        .spawn_background(SubagentRequest::new("host job"))
        .unwrap();
    assert_eq!(via_tool["spawnId"], 0);
    assert_eq!(typed.spawn_id, 1);
    assert_eq!(typed.status, BackgroundStatus::Running);
    assert_eq!(
        typed.clone().into_value(),
        json!({"spawnId": 1, "status": "running", "termination": "detached", "identity": null})
    );
    assert_eq!(tool.active_jobs().len(), 2);
    assert_eq!(manager.active().len(), 2);

    release.cancel();
    let mut completions = Vec::new();
    for _ in 0..2 {
        completions.push(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("completion notification timed out")
                .expect("completion channel closed"),
        );
    }
    completions.sort_by_key(|notification| notification.spawn.id);
    assert_eq!(completions[0].spawn.call_id, "t");
    assert_eq!(completions[1].spawn.call_id, "host:1");
    assert_eq!(completions[1].spawn.task, "host job");
    assert_eq!(
        completions[1].result.as_ref().unwrap()["answer"],
        "background complete"
    );
    assert!(tool.active_jobs().is_empty());
}

#[tokio::test]
async fn typed_spawns_queue_under_the_same_limit_as_the_tool() {
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
    .background(manager, move |notification| {
        let _ = tx.send(notification);
    });

    let first = tool.spawn_background(SubagentRequest::new("one")).unwrap();
    let second = tool.spawn_background(SubagentRequest::new("two")).unwrap();
    let third = tool
        .call(json!({"task": "three", "background": true}), &ctx())
        .await
        .unwrap();
    assert_eq!(first.status, BackgroundStatus::Running);
    assert_eq!(second.status, BackgroundStatus::Queued);
    assert_eq!(third["status"], "queued");
    let jobs = tool.active_jobs();
    assert_eq!(
        jobs.iter().map(|job| job.status).collect::<Vec<_>>(),
        [
            BackgroundStatus::Running,
            BackgroundStatus::Queued,
            BackgroundStatus::Queued
        ]
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 1);

    release.cancel();
    for _ in 0..3 {
        tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("completion notification timed out")
            .expect("completion channel closed");
    }
    assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[tokio::test]
async fn typed_routing_validation_matches_the_tool() {
    let model = Arc::new(ScriptedModel::new(vec![]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model.clone(), &ws).models([SubagentModel::new(
        "flash/test",
        "fast",
        model.clone(),
    )]);

    for requested in ["flash/test", "unknown/model"] {
        let via_tool = tool
            .call(json!({"task": "route", "model": requested}), &ctx())
            .await
            .unwrap_err()
            .to_string();
        let typed = tool
            .run_foreground(
                SubagentRequest::new("route").model(requested),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(typed, via_tool, "{requested}");
        assert!(typed.contains(requested), "{typed}");
    }
    assert_eq!(model.generate_calls(), 0, "rejected routes never run");
}

#[tokio::test]
async fn typed_background_spawn_is_rejected_like_the_tool_without_a_host() {
    let model = Arc::new(ScriptedModel::new(vec![]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model, &ws);
    assert!(!tool.has_background());

    let via_tool = tool
        .call(json!({"task": "no host", "background": true}), &ctx())
        .await
        .unwrap_err()
        .to_string();
    let typed = tool
        .spawn_background(SubagentRequest::new("no host"))
        .unwrap_err()
        .to_string();
    assert_eq!(typed, via_tool);
    assert!(tool.active_jobs().is_empty());
    assert!(!tool.cancel_job(0));
    assert_eq!(tool.cancel_all_jobs(), 0);
}

#[tokio::test]
async fn typed_run_nests_children_by_the_shared_depth() {
    let (ws, _dir) = temp_ws();
    let nested = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call("1", "subagent", json!({"task": "inner"}))]),
        ModelResponse::final_text("grandchild done"),
        ModelResponse::final_text("child done"),
    ]));
    let tool = SubagentTool::new(nested.clone(), &ws).max_depth(SubagentDepth::new(2));
    let outcome = tool
        .run_foreground(
            SubagentRequest::new("outer"),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome.answer, "child done");
    assert_eq!(nested.generate_calls(), 3, "grandchild must actually run");
    assert_eq!(outcome.steps, 2);
    assert_eq!(outcome.tool_calls, 1);

    let flat = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call("1", "subagent", json!({"task": "inner"}))]),
        ModelResponse::final_text("child done"),
    ]));
    let tool = SubagentTool::new(flat.clone(), &ws); // max_depth defaults to 1
    tool.run_foreground(
        SubagentRequest::new("outer"),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();
    let contexts = flat.observed_contexts();
    let saw_unknown = contexts
        .last()
        .unwrap()
        .messages()
        .iter()
        .any(|m| match m {
            Message::Tool { results } => results
                .iter()
                .any(|r| r.is_error && r.output.to_string().contains("unknown tool")),
            _ => false,
        });
    assert!(saw_unknown, "at the depth limit the child has no subagent tool");
}

#[tokio::test]
async fn typed_cancellation_matches_the_tool_cancel_action() {
    let release = CancellationToken::new();
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(3);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let spawns: Arc<Mutex<Vec<SubagentSpawn>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = spawns.clone();
    let tool = SubagentTool::new(
        Arc::new(GateModel {
            release: release.clone(),
        }),
        &ws,
    )
    .spawn_extensions(Arc::new(move |spawn| {
        recorded.lock().unwrap().push(spawn.clone());
        Vec::new()
    }))
    .background(manager, move |notification| {
        let _ = tx.send(notification);
    });

    let typed = tool.spawn_background(SubagentRequest::new("host")).unwrap();
    let via_tool = tool
        .call(json!({"task": "model", "background": true}), &ctx())
        .await
        .unwrap();
    let model_id = via_tool["spawnId"].as_u64().unwrap();
    {
        let spawns = spawns.lock().unwrap();
        assert_eq!(spawns.len(), 2, "host spawns announce to spawn extensions");
        assert_eq!(spawns[0].call_id, format!("host:{}", typed.spawn_id));
        assert_eq!(spawns[1].call_id, "t");
    }

    assert!(tool.cancel_job(typed.spawn_id));
    let host_cancelled = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("cancelled host job did not report")
        .expect("completion channel closed");
    assert_eq!(host_cancelled.spawn.id, typed.spawn_id);

    let cancelled = tool
        .call(json!({"action": "cancel", "spawnId": model_id}), &ctx())
        .await
        .unwrap();
    assert_eq!(cancelled["cancelled"], true);
    let model_cancelled = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("cancelled model job did not report")
        .expect("completion channel closed");
    assert_eq!(model_cancelled.spawn.id, model_id);

    let failure = |result: &Result<Value, String>| {
        let error = result.as_ref().unwrap_err();
        error
            .split(" (runtimeMs=")
            .next()
            .unwrap_or(error)
            .to_string()
    };
    assert_eq!(
        failure(&host_cancelled.result),
        failure(&model_cancelled.result)
    );
    assert!(tool.active_jobs().is_empty());
    assert!(!tool.cancel_job(typed.spawn_id), "finished jobs are unknown");

    tool.spawn_background(SubagentRequest::new("a")).unwrap();
    tool.spawn_background(SubagentRequest::new("b")).unwrap();
    assert_eq!(tool.cancel_all_jobs(), 2);
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("cancel_all completion timed out")
            .expect("completion channel closed");
    }
    assert!(tool.active_jobs().is_empty());
}
