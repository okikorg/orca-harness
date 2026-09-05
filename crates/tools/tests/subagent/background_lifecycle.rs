#[tokio::test]
async fn background_capacity_rejection_does_not_announce_or_start_a_worker() {
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(1).with_completion_capacity(0);
    let stats = orca_harness_tools::BackgroundStats::new();
    let tool = SubagentTool::new(Arc::new(StallModel), &ws)
        .stats(stats.clone())
        .spawn_extensions(Arc::new(|_| {
            panic!("rejected workers must not be announced")
        }))
        .background(manager.clone(), |_| {
            panic!("rejected workers cannot complete")
        });
    let error = tool
        .call(json!({"task": "rejected", "background": true}), &ctx())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("completion capacity reached"));
    assert!(manager.active().is_empty());
    assert_eq!(stats.agents(), 0);
}

#[tokio::test]
async fn dropping_background_owners_cancels_running_and_queued_workers() {
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(1);
    let stats = orca_harness_tools::BackgroundStats::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = SubagentTool::new(Arc::new(StallModel), &ws)
        .stats(stats.clone())
        .background(manager.clone(), move |notification| {
            let _ = tx.send(notification);
        });
    for task in ["running", "queued"] {
        tool.call(json!({"task": task, "background": true}), &ctx())
            .await
            .unwrap();
    }
    drop(manager);
    assert_eq!(stats.agents(), 2, "the tool still owns its workers");
    drop(tool);
    for _ in 0..2 {
        let completion = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("owner drop must cancel detached workers")
            .unwrap();
        assert!(completion.result.is_err());
    }
    tokio::task::yield_now().await;
    assert_eq!(stats.agents(), 0);
}

struct ShortWorkModel;
#[async_trait]
impl Model for ShortWorkModel {
    async fn generate(&self, _: &Context, _: &[ToolSchema]) -> Result<ModelResponse, ModelError> {
        tokio::time::sleep(Duration::from_millis(600)).await;
        Ok(ModelResponse::final_text("finished"))
    }
}

#[tokio::test]
async fn queued_background_workers_receive_their_full_execution_timeout() {
    let (ws, _dir) = temp_ws();
    let settings = orca_harness_tools::SubagentDepth::default();
    settings.set_timeout_secs(1);
    let manager = orca_harness_tools::SubagentManager::new(1);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = SubagentTool::new(Arc::new(ShortWorkModel), &ws)
        .max_depth(settings)
        .background(manager, move |notification| {
            let _ = tx.send(notification);
        });
    for task in ["first", "second"] {
        tool.call(json!({"task": task, "background": true}), &ctx())
            .await
            .unwrap();
    }
    for _ in 0..2 {
        let completion = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completion.result.unwrap()["answer"], "finished");
    }
}

struct PanicWorkerModel;
#[async_trait]
impl Model for PanicWorkerModel {
    async fn generate(&self, _: &Context, _: &[ToolSchema]) -> Result<ModelResponse, ModelError> {
        panic!("simulated worker panic")
    }
}

#[tokio::test]
async fn panicking_background_workers_report_failure_and_release_queue_slots() {
    let (ws, _dir) = temp_ws();
    let manager = orca_harness_tools::SubagentManager::new(1);
    let stats = orca_harness_tools::BackgroundStats::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tool = SubagentTool::new(Arc::new(PanicWorkerModel), &ws)
        .stats(stats.clone())
        .background(manager.clone(), move |notification| {
            let _ = tx.send(notification);
        });
    for task in ["first", "second"] {
        tool.call(json!({"task": task, "background": true}), &ctx())
            .await
            .unwrap();
    }
    for _ in 0..2 {
        let completion = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("panicking workers must report terminal results")
            .unwrap();
        assert!(completion.result.unwrap_err().contains("interrupted"));
    }
    assert!(manager.active().is_empty());
    assert_eq!(stats.agents(), 0);
}
