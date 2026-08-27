#[test]
fn default_governance_is_bounded() {
    assert_eq!(DEFAULT_SUBAGENT_MAX_STEPS, 24);
    assert_eq!(MAX_SUBAGENT_MAX_STEPS, 96);
    assert_eq!(DEFAULT_SUBAGENT_TIMEOUT, Duration::from_secs(5 * 60));
}

#[tokio::test]
async fn runs_task_and_reports_answer_and_usage() {
    let model = Arc::new(ScriptedModel::new(vec![ModelResponse::Final {
        text: "found 3 files".into(),
        usage: Some(Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        }),
    }]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model, &ws);
    let out = tool
        .call(json!({"task": "count files"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["answer"], "found 3 files");
    assert_eq!(out["usage"]["inputTokens"], 10);
    assert_eq!(out["usage"]["outputTokens"], 5);
    assert_eq!(out["steps"], 1);
    assert_eq!(out["toolCalls"], 0);
    assert_eq!(out["termination"], "completed");
    assert!(out["runtimeMs"].is_u64());
}

#[tokio::test]
async fn telemetry_counts_model_issued_unknown_tool_calls() {
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call("1", "missing_tool", json!({}))]),
        ModelResponse::final_text("recovered"),
    ]));
    let (ws, _dir) = temp_ws();
    let out = SubagentTool::new(model, &ws)
        .call(json!({"task": "recover"}), &ctx())
        .await
        .unwrap();

    assert_eq!(out["answer"], "recovered");
    assert_eq!(out["steps"], 2);
    assert_eq!(out["toolCalls"], 1);
}

#[tokio::test]
async fn inner_agent_executes_real_tools() {
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call(
            "1",
            "write_file",
            json!({"path": "note.txt", "content": "from the subagent"}),
        )],
        "wrote it",
    ));
    let (ws, dir) = temp_ws();
    let tool = SubagentTool::new(model, &ws);
    let out = tool
        .call(json!({"task": "write a note"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["answer"], "wrote it");
    assert_eq!(
        std::fs::read_to_string(dir.join("note.txt")).unwrap(),
        "from the subagent"
    );
}

/// A tool `shell`-shaped enough for the retry rule: reports failures as
/// data (`success: false`) rather than `Err`.
#[derive(Clone)]
struct FlakyShell {
    attempts: Arc<std::sync::atomic::AtomicU32>,
}

#[async_trait]
impl Tool for FlakyShell {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "shell".into(),
            description: "test shell".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let n = self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        // Fails as data twice, then succeeds — the inner-agent retry
        // (which honors the data-failure rule) must hide both failures.
        Ok(json!({"success": n >= 3, "stdout": format!("attempt {n}")}))
    }
}

#[tokio::test]
async fn inner_agents_retry_data_failing_tool_calls() {
    let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (_ws, _dir) = temp_ws();

    // One scripted turn: the inner agent issues one `shell` call whose
    // runner reports `success: false` twice before succeeding (all within
    // a single tool call — the inner retry wrapper re-invokes the tool),
    // then the agent answers. The two data failures must be invisible to
    // the inner loop's model.
    let tool = SubagentTool::with_tools(
        Arc::new(ScriptedModel::tool_round(
            vec![call("1", "shell", json!({"command": "probe"}))],
            "succeeded",
        )),
        {
            let attempts = attempts.clone();
            Arc::new(move || {
                vec![Arc::new(FlakyShell {
                    attempts: attempts.clone(),
                }) as Arc<dyn Tool>]
            })
        },
    )
    .retry_with_rule(3, Duration::from_millis(1), |call, out| {
        call.name == "shell" && out["success"].as_bool() == Some(false)
    });

    let out = tool.call(json!({"task": "probe"}), &ctx()).await.unwrap();
    assert_eq!(out["answer"], "succeeded");
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "two data failures must be retried before the third succeeds"
    );
}

/// A model that never answers — for cancellation tests.
struct StallModel;

#[async_trait]
impl Model for StallModel {
    async fn generate(
        &self,
        _context: &Context,
        _tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        tokio::time::sleep(Duration::from_secs(300)).await;
        Err(ModelError::InvalidResponse("unreachable".into()))
    }
}

#[tokio::test]
async fn parent_cancellation_reaches_the_inner_run() {
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(Arc::new(StallModel), &ws);
    let cancel = CancellationToken::new();
    let tctx = ToolContext {
        call_id: "t".into(),
        tool_name: "subagent".into(),
        cancellation: cancel.clone(),
        deadline: None,
    };
    let handle = tokio::spawn(async move { tool.call(json!({"task": "stall"}), &tctx).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("cancellation must end the inner run promptly")
        .unwrap();
    assert!(result.is_err());
}

#[tokio::test]
async fn parent_deadline_bounds_the_inner_run() {
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(Arc::new(StallModel), &ws);
    let tctx = ToolContext {
        call_id: "t".into(),
        tool_name: "subagent".into(),
        cancellation: CancellationToken::new(),
        deadline: Some(tokio::time::Instant::now() + Duration::from_millis(50)),
    };

    let err = tokio::time::timeout(
        Duration::from_secs(1),
        tool.call(json!({"task": "stall"}), &tctx),
    )
    .await
    .expect("the parent deadline must bound the worker")
    .unwrap_err()
    .to_string();
    assert!(err.contains("deadline exceeded"), "{err}");
    assert!(err.contains("steps=1"), "{err}");
}

#[tokio::test]
async fn step_limit_exhaustion_is_a_tool_error() {
    // One permitted step that returns tool calls: the run cannot finish.
    let model = Arc::new(ScriptedModel::new(vec![ModelResponse::tool_calls(vec![
        call("1", "list_dir", json!({"path": "."})),
    ])]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model, &ws).limits(Limits {
        max_steps: 1,
        ..Limits::default()
    });
    let result = tool.call(json!({"task": "loop forever"}), &ctx()).await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("step limit exceeded"), "{err}");
    assert!(err.contains("steps=1"), "{err}");
    assert!(err.contains("toolCalls=1"), "{err}");
}

#[tokio::test]
async fn task_is_required() {
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(Arc::new(StallModel), &ws);
    let err = tool.call(json!({}), &ctx()).await.unwrap_err();
    assert!(err.to_string().contains("task"));
}

#[tokio::test]
async fn selected_model_runs_while_omission_keeps_the_default() {
    let default = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text(
        "default",
    )]));
    let flash = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text("flash")]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(default.clone(), &ws)
        .inherited_identity("openrouter", "default/model")
        .models([
            SubagentModel::new("flash/test", "fast test model", flash.clone())
                .identity("openrouter", "vendor/flash-model"),
        ]);

    let selected = tool
        .call(json!({"task": "one", "model": "flash/test"}), &ctx())
        .await
        .unwrap();
    let inherited = tool.call(json!({"task": "two"}), &ctx()).await.unwrap();

    assert_eq!(selected["answer"], "flash");
    assert_eq!(selected["identity"]["provider"], "openrouter");
    assert_eq!(selected["identity"]["model"], "vendor/flash-model");
    assert_eq!(selected["identity"]["route"], "flash/test");
    assert_eq!(inherited["answer"], "default");
    assert_eq!(inherited["identity"]["provider"], "openrouter");
    assert_eq!(inherited["identity"]["model"], "default/model");
    assert!(inherited["identity"]["route"].is_null());
    assert_eq!(flash.generate_calls(), 1);
    assert_eq!(default.generate_calls(), 1);
}

#[tokio::test]
async fn unknown_model_fails_before_spawning() {
    let default = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text(
        "unused",
    )]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(default.clone(), &ws).models([SubagentModel::new(
        "flash/test",
        "fast test model",
        default.clone(),
    )]);

    let err = tool
        .call(json!({"task": "one", "model": "invented"}), &ctx())
        .await
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("unknown subagent model `invented`"));
    assert_eq!(default.generate_calls(), 0);
}

#[test]
fn schema_only_advertises_configured_model_ids() {
    let (ws, _dir) = temp_ws();
    let model = Arc::new(ScriptedModel::new(vec![]));
    let plain = SubagentTool::new(model.clone(), &ws);
    let description = plain.schema().description;
    assert!(description.contains("one bounded task"));
    assert!(description.contains("exact result expected"));
    assert!(description.contains("explicit stopping condition"));
    assert!(description.contains("Avoid open-ended goals"));
    assert!(plain.schema().parameters["properties"]["model"].is_null());

    let configured = SubagentTool::new(model.clone(), &ws).models([
        SubagentModel::new("flash/one", "fast", model.clone()),
        SubagentModel::new("frontier/two", "strong", model),
    ]);
    assert_eq!(
        configured.schema().parameters["properties"]["model"]["enum"],
        json!(["flash/one", "frontier/two"])
    );
    assert_eq!(configured.schema().parameters["required"], json!(["task"]));
}

use orca_harness_core::Message;
use orca_harness_tools::SubagentDepth;
