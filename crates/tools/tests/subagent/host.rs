// Typed host operations must behave exactly like the model tool: same
// routing validation, admission, spawn ids, nesting, and cancellation.

use orca_harness_tools::{BackgroundStatus, SubagentOutcome, SubagentRequest, WorkerTiming};

#[test]
fn timing_zero_defaults_have_a_stable_serialized_shape() {
    let value = SubagentOutcome {
        answer: "done".into(),
        input_tokens: 0,
        output_tokens: 0,
        reasoning_tokens: None,
        cache_read_tokens: 0,
        cache_create_tokens: 0,
        runtime_ms: 0,
        steps: 0,
        tool_calls: 0,
        timing: WorkerTiming::default(),
        identity: None,
    }
    .into_value();

    assert_eq!(
        value["timing"],
        json!({
            "modelCallElapsedMs": [],
            "modelCumulativeMs": 0,
            "toolCallElapsed": [],
            "toolCumulativeMs": 0,
        })
    );
}

fn scripted_final(text: &str) -> Arc<ScriptedModel> {
    Arc::new(ScriptedModel::new(vec![
        ModelResponse::Final {
            text: text.into(),
            usage: Some(Usage {
                input_tokens: 11,
                output_tokens: 7,
                reasoning_tokens: Some(3),
                cache_read_tokens: 19,
                cache_create_tokens: 23,
            }),
        },
        ModelResponse::Final {
            text: text.into(),
            usage: Some(Usage {
                input_tokens: 11,
                output_tokens: 7,
                reasoning_tokens: Some(3),
                cache_read_tokens: 19,
                cache_create_tokens: 23,
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
    assert_eq!(typed.reasoning_tokens, Some(3));
    assert_eq!(typed.cache_read_tokens, 19);
    assert_eq!(typed.cache_create_tokens, 23);
    assert_eq!(typed.identity.as_ref().unwrap().model, "worker");
    let mut typed_json = typed.into_value();
    let mut tool_json = via_tool;
    // Wall-clock fields legitimately differ between these separate runs.
    typed_json["runtimeMs"] = json!(0);
    tool_json["runtimeMs"] = json!(0);
    typed_json["timing"] = json!(null);
    tool_json["timing"] = json!(null);
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
    // Negative check: a too-short sleep can only pass spuriously, never fail.
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
    let spawns: Arc<Mutex<Vec<SubagentSpawn>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = spawns.clone();
    let tool = SubagentTool::new(nested.clone(), &ws)
        .max_depth(SubagentDepth::new(2))
        .spawn_extensions(Arc::new(move |spawn| {
            recorded.lock().unwrap().push(spawn.clone());
            Vec::new()
        }));
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
    {
        let spawns = spawns.lock().unwrap();
        assert_eq!(
            spawns.iter().map(|spawn| spawn.depth).collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(spawns[0].call_id, format!("host:{}", spawns[0].id));
        assert_eq!(spawns[1].parent_id, Some(spawns[0].id));
    }

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

#[tokio::test]
async fn typed_foreground_run_stops_when_its_token_is_cancelled() {
    let release = CancellationToken::new();
    let (ws, _dir) = temp_ws();
    let stats = BackgroundStats::new();
    let tool = SubagentTool::new(
        Arc::new(GateModel {
            release: release.clone(),
        }),
        &ws,
    )
    .stats(stats.clone());

    let cancel = CancellationToken::new();
    let run = tokio::spawn({
        let tool = tool.clone();
        let cancel = cancel.clone();
        async move {
            tool.run_foreground(SubagentRequest::new("held"), cancel, None)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(stats.agents(), 1, "the worker is in flight");
    cancel.cancel();

    let error = tokio::time::timeout(Duration::from_secs(1), run)
        .await
        .expect("cancellation must end the run")
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("subagent failed: cancelled"), "{error}");
    assert_eq!(stats.agents(), 0, "in-flight accounting unwinds on cancel");
    assert!(!release.is_cancelled(), "the model never finished on its own");
}

#[tokio::test]
async fn sidekick_follow_up_reuses_context_and_stop_is_terminal() {
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::final_text("first report"),
        ModelResponse::final_text("follow-up report"),
    ]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model.clone(), &ws)
        .background(orca_harness_tools::SubagentManager::new(0), |_| {});

    let first = tool
        .start_sidekick_foreground(SubagentRequest::new("inspect alpha"), CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(first.status.as_str(), "idle");
    assert_eq!(tool.sidekick_status(first.spawn_id).unwrap().as_str(), "idle");
    let follow_up = tool
        .sidekick_task_foreground(first.spawn_id, "check beta", CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(follow_up.answer, "follow-up report");
    let observed = model.observed_contexts();
    assert_eq!(observed.len(), 2);
    assert_eq!(observed[0].messages().len(), 2);
    assert_eq!(observed[1].messages().len(), 4, "first turn is retained");

    tool.stop_sidekick(first.spawn_id).unwrap();
    tool.stop_sidekick(first.spawn_id).unwrap();
    assert_eq!(
        tool.sidekick_status(first.spawn_id).unwrap().as_str(),
        "stopped"
    );
    let error = tool
        .sidekick_task_foreground(first.spawn_id, "too late", CancellationToken::new(), None)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "sidekick is stopped");
}

#[tokio::test]
async fn cancel_all_leaves_idle_sidekick_available() {
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(0);
    let tool = SubagentTool::new(scripted_final("report"), &ws)
        .background(manager.clone(), |_| {});
    let report = tool
        .start_sidekick_foreground(SubagentRequest::new("task"), CancellationToken::new(), None)
        .await
        .unwrap();

    assert_eq!(tool.cancel_all_jobs(), 0);
    assert_eq!(tool.sidekick_status(report.spawn_id).unwrap().as_str(), "idle");
}

#[tokio::test]
async fn persistent_background_true_returns_an_immediate_stable_handle() {
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(scripted_final("unused"), &ws)
        .background(orca_harness_tools::SubagentManager::new(0), |_| {});
    let acknowledgement = tool
        .call(
            json!({"task": "task", "persistent": true, "background": true}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(acknowledgement["spawnId"], 0);
    assert_eq!(acknowledgement["status"], "running");
    assert_eq!(acknowledgement["persistent"], true);
}

#[tokio::test]
async fn background_sidekick_returns_before_model_and_stop_guards_late_completion() {
    let release = CancellationToken::new();
    let (ws, _dir) = temp_ws();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = SubagentTool::new(
        Arc::new(GateModel {
            release: release.clone(),
        }),
        &ws,
    )
    .background(orca_harness_tools::SubagentManager::new(1), move |event| {
        let _ = tx.send(event);
    });

    let acknowledgement = tokio::time::timeout(
        Duration::from_millis(100),
        tool.call(json!({"task": "held", "persistent": true}), &ctx()),
    )
    .await
    .expect("persistent task must not await the model")
    .unwrap();
    let id = acknowledgement["spawnId"].as_u64().unwrap();
    assert_eq!(tool.sidekick_status(id).unwrap().as_str(), "busy");
    tool.stop_sidekick(id).unwrap();
    release.cancel();
    assert!(tokio::time::timeout(Duration::from_millis(100), rx.recv())
        .await
        .is_err());
    assert_eq!(tool.sidekick_status(id).unwrap().as_str(), "stopped");
}

#[tokio::test]
async fn background_sidekick_delivers_each_turn_with_one_handle_and_retained_context() {
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::final_text("first"),
        ModelResponse::final_text("second"),
    ]));
    let (ws, _dir) = temp_ws();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = SubagentTool::new(model.clone(), &ws).background(
        orca_harness_tools::SubagentManager::new(1),
        move |event| {
            let _ = tx.send(event);
        },
    );

    let first = tool.start_sidekick(SubagentRequest::new("one")).unwrap();
    let first_event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_event.spawn.id, first.spawn_id);
    assert_eq!(first_event.result.unwrap()["answer"], "first");
    assert_eq!(tool.sidekick_status(first.spawn_id).unwrap().as_str(), "idle");

    let second = tool.sidekick_task(first.spawn_id, "two").unwrap();
    assert_eq!(second.spawn_id, first.spawn_id);
    let second_event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second_event.spawn.id, first.spawn_id);
    assert_eq!(second_event.result.unwrap()["answer"], "second");
    assert!(rx.try_recv().is_err(), "one completion per turn");
    assert_eq!(model.observed_contexts()[1].messages().len(), 4);
}

#[tokio::test]
async fn queued_sidekick_is_busy_and_can_be_stopped_without_starting() {
    let release = CancellationToken::new();
    let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(1);
    let tool = SubagentTool::new(
        Arc::new(CountingGateModel {
            started: started.clone(),
            release: release.clone(),
        }),
        &ws,
    )
    .background(manager, |_| {});
    let blocker = tool.spawn_background(SubagentRequest::new("block slot")).unwrap();
    let sidekick = tool.start_sidekick(SubagentRequest::new("queued")).unwrap();
    assert_eq!(sidekick.status, BackgroundStatus::Queued);
    assert_eq!(
        tool.sidekick_task(sidekick.spawn_id, "overlap")
            .unwrap_err()
            .to_string(),
        "sidekick is busy"
    );
    tool.stop_sidekick(sidekick.spawn_id).unwrap();
    release.cancel();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(tool.cancel_job(blocker.spawn_id) || tool.active_jobs().is_empty());
}

#[tokio::test]
async fn background_initial_failure_is_delivered_and_handle_can_retry() {
    struct FailOnceModel(std::sync::atomic::AtomicUsize);
    #[async_trait]
    impl Model for FailOnceModel {
        async fn generate(
            &self,
            _context: &Context,
            _tools: &[ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Err(ModelError::InvalidResponse("first failed".into()))
            } else {
                Ok(ModelResponse::final_text("retry succeeded"))
            }
        }
    }

    let (ws, _dir) = temp_ws();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = SubagentTool::new(
        Arc::new(FailOnceModel(std::sync::atomic::AtomicUsize::new(0))),
        &ws,
    )
    .background(orca_harness_tools::SubagentManager::new(1), move |event| {
        let _ = tx.send(event);
    });

    let acknowledgement = tool.start_sidekick(SubagentRequest::new("fail")).unwrap();
    let failed = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(failed.result.unwrap_err().contains("first failed"));
    assert_eq!(
        tool.sidekick_status(acknowledgement.spawn_id)
            .unwrap()
            .as_str(),
        "idle"
    );

    tool.sidekick_task(acknowledgement.spawn_id, "retry")
        .unwrap();
    let retried = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retried.result.unwrap()["answer"], "retry succeeded");
}

#[tokio::test]
async fn sidekick_rejects_busy_and_stop_cancels_active_task() {
    let release = CancellationToken::new();
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(
        Arc::new(GateModel {
            release: release.clone(),
        }),
        &ws,
    )
    .background(orca_harness_tools::SubagentManager::new(0), |_| {});
    // Seed the handle without waiting for its gated first model call.
    let run = tokio::spawn({
        let tool = tool.clone();
        async move {
            tool.start_sidekick_foreground(SubagentRequest::new("held"), CancellationToken::new(), None)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let error = tool
        .sidekick_task_foreground(0, "overlap", CancellationToken::new(), None)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "sidekick is busy");
    tool.stop_sidekick(0).unwrap();
    assert!(run.await.unwrap().unwrap_err().to_string().contains("cancelled"));
    assert!(!release.is_cancelled());
}

#[tokio::test]
async fn aborting_initial_sidekick_future_removes_hidden_registration() {
    let release = CancellationToken::new();
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(0);
    let tool = SubagentTool::new(
        Arc::new(GateModel {
            release: release.clone(),
        }),
        &ws,
    )
    .background(manager.clone(), |_| {});
    let run = tokio::spawn({
        let tool = tool.clone();
        async move {
            tool.start_sidekick_foreground(SubagentRequest::new("held"), CancellationToken::new(), None)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    run.abort();
    assert!(run.await.unwrap_err().is_cancelled());
    tokio::task::yield_now().await;

    assert_eq!(manager.live_workers(), 0);
    assert_eq!(
        tool.sidekick_status(0).unwrap_err().to_string(),
        "unknown sidekick handle"
    );
}

#[tokio::test]
async fn manager_reset_stops_idle_sidekicks() {
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(0);
    let tool = SubagentTool::new(scripted_final("report"), &ws)
        .background(manager.clone(), |_| {});
    let report = tool
        .start_sidekick_foreground(SubagentRequest::new("task"), CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(manager.cancel_all(), 0, "ordinary cancellation leaves sidekicks intact");
    assert_eq!(tool.sidekick_status(report.spawn_id).unwrap().as_str(), "idle");
    manager.reset();
    assert_eq!(
        tool.sidekick_status(report.spawn_id).unwrap().as_str(),
        "stopped"
    );
}

#[tokio::test]
async fn failed_initial_sidekick_task_cleans_up_the_undisclosed_handle() {
    let release = CancellationToken::new();
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(
        Arc::new(GateModel {
            release: release.clone(),
        }),
        &ws,
    )
    .background(orca_harness_tools::SubagentManager::new(0), |_| {});
    let cancel = CancellationToken::new();
    let first = tokio::spawn({
        let tool = tool.clone();
        let cancel = cancel.clone();
        async move {
            tool.start_sidekick_foreground(SubagentRequest::new("first"), cancel, None)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancel.cancel();
    assert!(first.await.unwrap().unwrap_err().to_string().contains("cancelled"));
    assert_eq!(
        tool.sidekick_status(0).unwrap_err().to_string(),
        "unknown sidekick handle"
    );
    assert_eq!(
        tool.sidekick_status(999).unwrap_err().to_string(),
        "unknown sidekick handle"
    );
}

#[tokio::test]
async fn dropping_followup_during_pending_model_clears_busy_and_manager_accounting() {
    struct FollowupGateModel {
        calls: std::sync::atomic::AtomicUsize,
        entered: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl Model for FollowupGateModel {
        async fn generate(
            &self,
            _context: &Context,
            _tools: &[ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            if self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                Ok(ModelResponse::final_text("initial"))
            } else {
                self.entered.notify_one();
                std::future::pending().await
            }
        }
    }

    let entered = Arc::new(tokio::sync::Notify::new());
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(1);
    let tool = SubagentTool::new(
        Arc::new(FollowupGateModel {
            calls: std::sync::atomic::AtomicUsize::new(0),
            entered: entered.clone(),
        }),
        &ws,
    )
    .background(manager.clone(), |_| {});
    let id = tool
        .start_sidekick_foreground(SubagentRequest::new("initial"), CancellationToken::new(), None)
        .await
        .unwrap()
        .spawn_id;
    let followup = tokio::spawn({
        let tool = tool.clone();
        async move {
            tool.sidekick_task_foreground(id, "pending model", CancellationToken::new(), None)
                .await
        }
    });
    entered.notified().await;
    followup.abort();
    assert!(followup.await.unwrap_err().is_cancelled());
    tokio::task::yield_now().await;

    assert_eq!(tool.sidekick_status(id).unwrap().as_str(), "idle");
    assert_eq!(manager.live_workers(), 0);
}

#[tokio::test]
async fn dropping_followup_during_pending_tool_rolls_back_and_clears_busy() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let entered_for_tool = entered.clone();
    let tools = Arc::new(move || {
        let entered = entered_for_tool.clone();
        vec![Arc::new(orca_harness_core::FnTool::new(
            "hold",
            "hold until dropped",
            json!({"type": "object"}),
            move |_args, _ctx| {
                let entered = entered.clone();
                async move {
                    entered.notify_one();
                    std::future::pending().await
                }
            },
        )) as Arc<dyn Tool>]
    });
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::final_text("initial"),
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call("held", "hold", json!({}))],
            usage: None,
        },
        ModelResponse::final_text("after rollback"),
    ]));
    let manager = orca_harness_tools::SubagentManager::new(1);
    let tool = SubagentTool::with_tools(model.clone(), tools).background(manager.clone(), |_| {});
    let id = tool
        .start_sidekick_foreground(SubagentRequest::new("initial"), CancellationToken::new(), None)
        .await
        .unwrap()
        .spawn_id;
    let followup = tokio::spawn({
        let tool = tool.clone();
        async move {
            tool.sidekick_task_foreground(id, "pending tool", CancellationToken::new(), None)
                .await
        }
    });
    entered.notified().await;
    followup.abort();
    assert!(followup.await.unwrap_err().is_cancelled());
    tokio::task::yield_now().await;
    assert_eq!(tool.sidekick_status(id).unwrap().as_str(), "idle");
    assert_eq!(manager.live_workers(), 0);

    let report = tool
        .sidekick_task_foreground(id, "retry", CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(report.answer, "after rollback");
    assert_eq!(model.observed_contexts()[2].messages().len(), 4);
}

#[tokio::test]
async fn followup_gets_a_fresh_timeout_window() {
    let settings = orca_harness_tools::SubagentDepth::default();
    settings.set_timeout_secs(1);
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::final_text("initial"),
        ModelResponse::final_text("followup"),
    ]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model, &ws)
        .max_depth(settings)
        .background(orca_harness_tools::SubagentManager::new(1), |_| {});
    let id = tool
        .start_sidekick_foreground(SubagentRequest::new("initial"), CancellationToken::new(), None)
        .await
        .unwrap()
        .spawn_id;

    tokio::time::sleep(Duration::from_millis(1100)).await;
    let report = tool
        .sidekick_task_foreground(id, "after initial timeout window", CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(report.answer, "followup");
}
