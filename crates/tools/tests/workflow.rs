use orca_harness_core::{
    CancellationToken, Context, Model, ModelError, ModelResponse, Tool, ToolSchema,
};
use orca_harness_tools::{
    BackgroundStatus, SubagentManager, SubagentTool, WorkflowStore, WorkflowTool,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
mod workflow_support;
use workflow_support::{ctx, terminal, tool, wait_until, Echo};
#[tokio::test]
async fn fanout_reuses_one_slot_and_one_delivery_reservation_and_replays() {
    let (tool, manager, mut rx, calls, _store) = tool(None, 1);
    let graph = json!([
        {"id":"source","prompt":"dimensions","schema":"string[]"},
        {"id":"map","kind":"map","over":"source","prompt":"review {{ item }}"},
        {"id":"report","needs":["map"],"prompt":"report {{ stages.map.output }}"}
    ]);
    let ack = tool
        .call(json!({"action":"run","graph":graph}), &ctx())
        .await
        .unwrap();
    let (notification, stages) = terminal(&mut rx).await;
    assert_eq!(stages, 4);
    assert_eq!(
        notification.result.as_ref().unwrap()["workflow"]["peakRunning"],
        1
    );
    assert!(notification.result.is_ok());
    assert!(manager.active().is_empty());
    assert_eq!(calls.lock().unwrap().len(), 4);
    assert!(
        tool.call(json!({"action":"run","graph":graph}), &ctx())
            .await
            .is_err(),
        "terminal reservation remains until consumed"
    );
    manager.acknowledge(notification.generation, notification.spawn.id);
    let output = tool
        .call(
            json!({"action":"output","runId":ack["runId"],"stage":"map"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(output["answer"], "[\"review a\",\"review b\"]");
    let replay = tool
        .call(
            json!({"action":"run","graph":graph,"resumeFrom":ack["runId"]}),
            &ctx(),
        )
        .await
        .unwrap();
    let (n, stages) = terminal(&mut rx).await;
    assert_eq!(stages, 0);
    assert!(n.result.is_ok());
    assert_eq!(calls.lock().unwrap().len(), 4);
    manager.acknowledge(n.generation, n.spawn.id);
    let mut changed = graph;
    changed[1]["prompt"] = json!("changed {{ item }}");
    tool.call(
        json!({"action":"run","graph":changed,"resumeFrom":replay["runId"]}),
        &ctx(),
    )
    .await
    .unwrap();
    let (n, stages) = terminal(&mut rx).await;
    assert_eq!(stages, 3);
    assert!(n.result.is_ok());
}
#[tokio::test]
async fn cancellation_and_global_reset_settle_once_and_clear_runs() {
    let (tool, manager, mut rx, _, _store) = tool(Some(CancellationToken::new()), 1);
    let graph = json!([{"id":"a","prompt":"A"},{"id":"b","prompt":"B"}]);
    let ack = tool
        .call(json!({"action":"run","graph":graph}), &ctx())
        .await
        .unwrap();
    tool.call(json!({"action":"cancel","runId":ack["runId"]}), &ctx())
        .await
        .unwrap();
    let (n, _) = terminal(&mut rx).await;
    assert!(n.result.is_err());
    manager.acknowledge(n.generation, n.spawn.id);
    let ack = tool
        .call(json!({"action":"run","graph":graph}), &ctx())
        .await
        .unwrap();
    wait_until(
        || {
            manager.active().iter().any(|job| {
                job.spawn.run == ack["runId"].as_u64() && job.status == BackgroundStatus::Running
            })
        },
        "a workflow stage to hold a running slot",
    )
    .await;
    manager.cancel_all();
    assert_eq!(
        tool.call(json!({"action":"list"}), &ctx()).await.unwrap()["runs"],
        json!([])
    );
    assert!(tool.call(json!({"action":"list"}), &ctx()).await.is_err());
    assert!(!manager
        .active()
        .iter()
        .any(|job| job.spawn.id == ack["runId"].as_u64().unwrap()));
    let (n, _) = terminal(&mut rx).await;
    assert!(!manager.is_current(n.generation));
    let outcome: serde_json::Value = serde_json::from_str(
        n.result
            .unwrap_err()
            .split_once('\n')
            .expect("cancelled outcome follows the error")
            .1,
    )
    .unwrap();
    assert_eq!(
        outcome["peakRunning"], 1,
        "cancel_all preserves the run's telemetry before clearing manager jobs"
    );
}
#[tokio::test]
async fn invalid_graph_or_model_has_no_admissions() {
    let (tool, manager, _, calls, store) = tool(None, 1);
    assert!(tool
        .call(
            json!({"action":"run","graph":[{"id":"a","prompt":"a","model":"unknown"}]}),
            &ctx()
        )
        .await
        .is_err());
    assert!(manager.active().is_empty());
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(store.runs(), 0, "a rejected run records nothing");
}
#[tokio::test]
async fn more_than_completion_capacity_stages_do_not_exhaust_admission() {
    let (tool, manager, mut rx, _, _store) = tool(None, 1);
    let graph: Vec<_> = (0..150)
        .map(|n| json!({"id":format!("s{n}"),"prompt":"x"}))
        .collect();
    tool.call(json!({"action":"run","graph":graph}), &ctx())
        .await
        .unwrap();
    let (n, stages) = terminal(&mut rx).await;
    assert_eq!(stages, 150);
    assert!(n.result.is_ok());
    assert!(manager.active().is_empty());
}

#[tokio::test]
async fn dropping_session_owners_cancels_workflow_workers() {
    let (tool, manager, mut rx, _, _store) = tool(Some(CancellationToken::new()), 1);
    tool.call(
        json!({"action":"run","graph":[{"id":"a","prompt":"held"}]}),
        &ctx(),
    )
    .await
    .unwrap();
    drop(tool);
    drop(manager);
    let (n, _) = terminal(&mut rx).await;
    assert!(n.result.is_err());
}
#[tokio::test]
async fn rebuilt_tool_can_list_and_cancel_existing_run() {
    let (first, manager, mut rx, calls, store) = tool(Some(CancellationToken::new()), 1);
    let ack = first
        .call(
            json!({"action":"run","graph":[{"id":"a","prompt":"held"}]}),
            &ctx(),
        )
        .await
        .unwrap();
    let subagent = Arc::new(
        SubagentTool::with_tools(Echo { calls, held: None }, Arc::new(Vec::new))
            .background(manager.clone(), |_| {}),
    );
    let rebuilt = WorkflowTool::new(subagent, store.clone()).unwrap();
    drop(first);
    let list = rebuilt
        .call(json!({"action":"list"}), &ctx())
        .await
        .unwrap();
    assert_eq!(list["runs"][0]["runId"], ack["runId"]);
    rebuilt
        .call(json!({"action":"cancel","runId":ack["runId"]}), &ctx())
        .await
        .unwrap();
    assert!(terminal(&mut rx).await.0.result.is_err());
}
#[tokio::test]
async fn run_deadline_covers_running_and_queued_stages() {
    let (tool, manager, mut rx, _, _store) = tool(Some(CancellationToken::new()), 1);
    tool.call(json!({"action":"run","timeoutSeconds":1,"graph":[{"id":"a","prompt":"held"},{"id":"b","prompt":"queued"}]}),&ctx()).await.unwrap();
    let (n, _) = terminal(&mut rx).await;
    assert!(n.result.is_err());
    manager.cancel_all();
}

#[tokio::test]
async fn map_chains_and_replayed_sources_create_real_three_level_spawn_trees() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let spawns = Arc::new(Mutex::new(Vec::new()));
    let observed = spawns.clone();
    let manager = SubagentManager::new(2);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let subagent = Arc::new(
        SubagentTool::with_tools(Echo { calls, held: None }, Arc::new(Vec::new))
            .background(manager, move |n| {
                let _ = tx.send(n);
            })
            .spawn_extensions(Arc::new(move |spawn| {
                observed.lock().unwrap().push(spawn.clone());
                Vec::new()
            })),
    );
    let store = WorkflowStore::new();
    let tool = WorkflowTool::new(subagent, store).unwrap();
    let graph = json!([
        {"id":"source","prompt":"dimensions","schema":"string[]"},
        {"id":"review","kind":"map","over":"source","prompt":"review {{ item }}"},
        {"id":"verify","kind":"map","over":"review","prompt":"verify {{ item }}"}
    ]);
    let mut prior = None;
    for cached in [false, true] {
        spawns.lock().unwrap().clear();
        let mut input = json!({"action":"run","graph":graph});
        if let Some(id) = prior {
            input["resumeFrom"] = json!(id);
        }
        let ack = tool.call(input, &ctx()).await.unwrap();
        prior = ack["runId"].as_u64();
        let (n, count) = terminal(&mut rx).await;
        assert!(n.result.is_ok());
        assert_eq!(count, if cached { 0 } else { 5 });
        let spawns = spawns.lock().unwrap();
        let source = spawns.iter().find(|s| s.task == "dimensions").unwrap();
        for item in ["a", "b"] {
            let review = spawns
                .iter()
                .find(|s| s.task == format!("review {item}"))
                .unwrap();
            let verify = spawns
                .iter()
                .find(|s| s.task == format!("verify review {item}"))
                .unwrap();
            assert_eq!(review.parent_id, Some(source.id));
            assert_eq!(review.depth, 2);
            assert_eq!(verify.parent_id, Some(review.id));
            assert_eq!(verify.depth, 3);
        }
    }
}
#[tokio::test]
async fn route_changes_during_execution_cannot_store_a_result_under_the_new_model_key() {
    use orca_harness_tools::{SubagentDepth, SubagentModel};
    let settings = SubagentDepth::default();
    let a_calls = Arc::new(Mutex::new(Vec::new()));
    let b_calls = Arc::new(Mutex::new(Vec::new()));
    let release = CancellationToken::new();
    let a = Echo {
        calls: a_calls.clone(),
        held: Some(release.clone()),
    };
    let b = Echo {
        calls: b_calls.clone(),
        held: None,
    };
    let manager = SubagentManager::from_settings(settings.clone());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let subagent = Arc::new(
        SubagentTool::with_tools(b.clone(), Arc::new(Vec::new))
            .models([
                SubagentModel::new("local/a", "a", a),
                SubagentModel::new("local/b", "b", b),
            ])
            .max_depth(settings.clone())
            .background(manager, move |n| {
                let _ = tx.send(n);
            }),
    );
    assert!(settings.set_default_model(Some("local/a".into())));
    let store = WorkflowStore::new();
    let tool = WorkflowTool::new(subagent, store).unwrap();
    let graph = json!([{"id":"a","prompt":"task"}]);
    let ack = tool
        .call(json!({"action":"run","graph":graph}), &ctx())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while a_calls.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(settings.set_default_model(Some("local/b".into())));
    release.cancel();
    assert!(terminal(&mut rx).await.0.result.is_ok());
    tool.call(
        json!({"action":"run","graph":graph,"resumeFrom":ack["runId"]}),
        &ctx(),
    )
    .await
    .unwrap();
    assert!(terminal(&mut rx).await.0.result.is_ok());
    assert_eq!(b_calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn completed_map_output_is_persisted_while_downstream_is_still_running() {
    #[derive(Clone)]
    struct HeldReport {
        inner: Echo,
        started: CancellationToken,
        release: CancellationToken,
    }
    #[async_trait::async_trait]
    impl Model for HeldReport {
        async fn generate(
            &self,
            context: &Context,
            schemas: &[ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            if context.messages().iter().any(|m| matches!(m,orca_harness_core::Message::User{content,..} if content.starts_with("report"))) {
                self.started.cancel(); self.release.cancelled().await;
            }
            self.inner.generate(context, schemas).await
        }
    }
    let started = CancellationToken::new();
    let release = CancellationToken::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let subagent = Arc::new(
        SubagentTool::with_tools(
            HeldReport {
                inner: Echo {
                    calls: Arc::default(),
                    held: None,
                },
                started: started.clone(),
                release: release.clone(),
            },
            Arc::new(Vec::new),
        )
        .background(SubagentManager::new(1), move |n| {
            let _ = tx.send(n);
        }),
    );
    let store = WorkflowStore::new();
    let tool = WorkflowTool::new(subagent.clone(), store.clone()).unwrap();
    let ack = tool
        .call(
            json!({"action":"run","graph":[
                {"id":"source","prompt":"dimensions","schema":"string[]"},
                {"id":"map","kind":"map","over":"source","prompt":"review {{ item }}"},
                {"id":"report","needs":["map"],"prompt":"report {{ stages.map.output }}"}
            ]}),
            &ctx(),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), started.cancelled())
        .await
        .unwrap();
    let rebuilt = WorkflowTool::new(subagent, store.clone()).unwrap();
    let output = rebuilt
        .call(
            json!({"action":"output","runId":ack["runId"],"stage":"map"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(output["answer"], "[\"review a\",\"review b\"]");
    release.cancel();
    assert!(terminal(&mut rx).await.0.result.is_ok());
}

/// Non-instant stages: the `Echo` model above returns without yielding, so
/// nothing else covers a fan-out that actually interleaves in the runtime.
#[derive(Clone)]
struct Slow;
#[async_trait::async_trait]
impl Model for Slow {
    async fn generate(
        &self,
        context: &Context,
        _: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        let task = context
            .messages()
            .iter()
            .rev()
            .find_map(|m| match m {
                orca_harness_core::Message::User { content, .. } => Some(content.clone()),
                _ => None,
            })
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        Ok(ModelResponse::final_text(format!("answer for {task}")))
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn diamond_of_slow_stages_completes_without_stalling() {
    let manager = SubagentManager::new(100);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let subagent = Arc::new(
        SubagentTool::with_tools(Slow, Arc::new(Vec::new)).background(manager, move |n| {
            let _ = tx.send(n);
        }),
    );
    let tool = WorkflowTool::new(subagent, WorkflowStore::new()).unwrap();
    let graph = json!([
        {"id":"discover","prompt":"list files"},
        {"id":"count_src","needs":["discover"],"prompt":"src {{ stages.discover.output }}"},
        {"id":"count_test","needs":["discover"],"prompt":"test {{ stages.discover.output }}"},
        {"id":"summary","needs":["count_src","count_test"],
         "prompt":"sum {{ stages.count_src.output }} {{ stages.count_test.output }}"}
    ]);
    tool.call(json!({"action":"run","graph":graph}), &ctx())
        .await
        .unwrap();
    let (n, stages) = terminal(&mut rx).await;
    assert_eq!(stages, 4);
    let outcome = &n.result.as_ref().unwrap()["workflow"];
    assert_eq!(outcome["state"], "done");
    assert_eq!(
        outcome["peakRunning"], 2,
        "the two counters run in parallel"
    );
    assert!(outcome["outputs"]["summary"].is_string());
}
#[tokio::test]
async fn a_failed_stage_leads_with_its_error_and_does_not_read_as_cancelled() {
    let (tool, _manager, mut rx, _, _store) = tool(None, 4);
    // `source` declares string[] but Echo answers with the prompt text, so the
    // stage fails schema parsing after its one retry.
    let graph = json!([
        {"id":"source","prompt":"not json","schema":"string[]"},
        {"id":"after","needs":["source"],"prompt":"x {{ stages.source.output }}"}
    ]);
    tool.call(json!({"action":"run","graph":graph}), &ctx())
        .await
        .unwrap();
    let (n, _) = terminal(&mut rx).await;
    let error = n.result.unwrap_err();
    assert!(error.starts_with("workflow failed"), "{error}");
    assert!(error.contains("source:"), "{error}");
    let outcome: serde_json::Value =
        serde_json::from_str(error.split_once('\n').unwrap().1).unwrap();
    assert_eq!(outcome["stages"]["source"], "failed");
    assert_eq!(
        outcome["stages"]["after"], "stopped",
        "a stage abandoned by a sibling failure is not a cancellation"
    );
}
