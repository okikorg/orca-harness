//! Subagent tool driven by the scripted model — no network, fully
//! deterministic.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    CancellationToken, Context, Limits, Model, ModelError, ModelResponse, Tool, ToolContext,
    ToolError, ToolSchema, Usage,
};
use orca_harness_tools::{
    SubagentModel, SubagentTool, Workspace, DEFAULT_SUBAGENT_MAX_STEPS, DEFAULT_SUBAGENT_TIMEOUT,
};

static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn temp_ws() -> (Workspace, std::path::PathBuf) {
    let n = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("orca-harness-sub-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    (Workspace::new(dir.clone()), dir)
}

fn ctx() -> ToolContext {
    ToolContext {
        call_id: "t".into(),
        tool_name: "subagent".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

#[test]
fn default_governance_is_bounded() {
    assert_eq!(DEFAULT_SUBAGENT_MAX_STEPS, 12);
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

#[tokio::test]
async fn depth_two_lets_a_subagent_spawn_a_grandchild() {
    // One shared script, consumed strictly in order:
    //   child agent step 1 -> calls subagent
    //   grandchild agent   -> final
    //   child agent step 2 -> final
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call("1", "subagent", json!({"task": "inner"}))]),
        ModelResponse::final_text("grandchild done"),
        ModelResponse::final_text("child done"),
    ]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model.clone(), &ws).max_depth(SubagentDepth::new(2));
    let out = tool.call(json!({"task": "outer"}), &ctx()).await.unwrap();
    assert_eq!(out["answer"], "child done");
    assert_eq!(
        model.generate_calls(),
        3,
        "grandchild must actually have run"
    );
}

#[tokio::test]
async fn nested_subagents_inherit_selected_model_and_choices() {
    let default = Arc::new(ScriptedModel::new(vec![]));
    let flash = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call("1", "subagent", json!({"task": "inner"}))]),
        ModelResponse::final_text("grandchild on inherited flash"),
        ModelResponse::final_text("child done"),
    ]));
    let (ws, _dir) = temp_ws();
    let spawns: Arc<Mutex<Vec<SubagentSpawn>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = spawns.clone();
    let tool = SubagentTool::new(default.clone(), &ws)
        .models([SubagentModel::new("flash/test", "fast", flash.clone())
            .identity("openrouter", "vendor/flash")])
        .max_depth(SubagentDepth::new(2))
        .spawn_extensions(Arc::new(move |spawn| {
            recorded.lock().unwrap().push(spawn.clone());
            Vec::new()
        }));

    let out = tool
        .call(json!({"task": "outer", "model": "flash/test"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["answer"], "child done");
    assert_eq!(default.generate_calls(), 0);
    assert_eq!(flash.generate_calls(), 3);
    let spawns = spawns.lock().unwrap();
    assert_eq!(spawns.len(), 2);
    assert!(spawns.iter().all(|spawn| {
        spawn.identity.as_ref().is_some_and(|identity| {
            identity.provider == "openrouter"
                && identity.model == "vendor/flash"
                && identity.route.as_deref() == Some("flash/test")
        })
    }));
}

#[tokio::test]
async fn default_depth_gives_children_no_subagent_tool() {
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call("1", "subagent", json!({"task": "inner"}))]),
        ModelResponse::final_text("child done"),
    ]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model.clone(), &ws); // max_depth defaults to 1
    let out = tool.call(json!({"task": "outer"}), &ctx()).await.unwrap();
    assert_eq!(out["answer"], "child done");
    // The child's subagent call must have come back as an unknown tool.
    let contexts = model.observed_contexts();
    let last = contexts.last().unwrap();
    let saw_unknown = last.messages().iter().any(|m| match m {
        Message::Tool { results } => results
            .iter()
            .any(|r| r.is_error && r.output.to_string().contains("unknown tool")),
        _ => false,
    });
    assert!(saw_unknown, "child had no subagent tool, call must error");
}

#[tokio::test]
async fn raising_the_shared_depth_applies_to_the_next_spawn() {
    let depth = SubagentDepth::new(1);
    let model = Arc::new(ScriptedModel::new(vec![
        // First call (depth 1): nested attempt fails as unknown tool.
        ModelResponse::tool_calls(vec![call("1", "subagent", json!({"task": "inner"}))]),
        ModelResponse::final_text("first done"),
        // Second call (after raise to 2): nesting works.
        ModelResponse::tool_calls(vec![call("2", "subagent", json!({"task": "inner"}))]),
        ModelResponse::final_text("grandchild done"),
        ModelResponse::final_text("second done"),
    ]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model.clone(), &ws).max_depth(depth.clone());

    let out = tool.call(json!({"task": "one"}), &ctx()).await.unwrap();
    assert_eq!(out["answer"], "first done");

    assert_eq!(depth.set(2), 2);
    let out = tool.call(json!({"task": "two"}), &ctx()).await.unwrap();
    assert_eq!(out["answer"], "second done");
    assert_eq!(model.generate_calls(), 5);
}

#[test]
fn settings_handle_clamps_live_governance() {
    let settings = SubagentDepth::new(1);
    assert_eq!(settings.set_max_steps(0), 1);
    assert_eq!(settings.set_max_steps(99), 99);
    assert_eq!(settings.set_timeout_secs(1), 30);
    assert_eq!(settings.set_timeout_secs(99_999), 99_999);
    assert_eq!(settings.set_output_chars(1), 1_000);
    assert_eq!(settings.set_output_chars(99_999), 99_999);
    assert_eq!(settings.set_tool_attempts(0), 1);
    assert_eq!(settings.set_tool_attempts(99), 99);
    assert_eq!(settings.set_retry_backoff_ms(99_999), 99_999);
    settings.ensure_retry_defaults(3, 250);
    assert_eq!(
        settings.tool_attempts(),
        99,
        "defaults do not replace live choices"
    );
    assert_eq!(settings.retry_backoff_ms(), 99_999);
}

#[tokio::test]
async fn explicit_builders_survive_later_shared_settings_attachment() {
    let model = Arc::new(ScriptedModel::new(vec![ModelResponse::tool_calls(vec![
        call("1", "list_dir", json!({"path": "."})),
    ])]));
    let settings = SubagentDepth::new(1);
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model, &ws)
        .limits(Limits {
            max_steps: 1,
            ..Limits::default()
        })
        .max_depth(settings.clone());
    let err = tool
        .call(json!({"task": "loop"}), &ctx())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("step limit exceeded"), "{err}");
    assert_eq!(settings.max_steps(), 1);

    let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let tool = SubagentTool::with_tools(
        Arc::new(ScriptedModel::tool_round(
            vec![call("2", "shell", json!({}))],
            "done",
        )),
        {
            let attempts = attempts.clone();
            Arc::new(move || {
                vec![Arc::new(FlakyShell {
                    attempts: attempts.clone(),
                })]
            })
        },
    )
    .retry_with_rule(3, Duration::from_millis(1), |_, out| {
        out["success"].as_bool() == Some(false)
    })
    .max_depth(SubagentDepth::new(1));
    tool.call(json!({"task": "retry"}), &ctx()).await.unwrap();
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[test]
fn catalog_rebuild_preserves_valid_tier_choices_and_reconciles_removed_ones() {
    let settings = SubagentDepth::new(1);
    let model = Arc::new(ScriptedModel::new(vec![]));
    let (ws, _dir) = temp_ws();
    let _first = SubagentTool::new(model.clone(), &ws)
        .models([
            SubagentModel::new("local/a", "a", model.clone()),
            SubagentModel::new("local/b", "b", model.clone()),
            SubagentModel::new("flash/a", "f", model.clone()),
        ])
        .max_depth(settings.clone());
    assert!(settings.set_preferred_model("local", "local/b".into()));
    assert!(settings.set_model_route(Some("local".into())));

    let _same = SubagentTool::new(model.clone(), &ws)
        .models([
            SubagentModel::new("local/a", "a", model.clone()),
            SubagentModel::new("local/b", "b", model.clone()),
        ])
        .max_depth(settings.clone());
    assert_eq!(settings.model_route().as_deref(), Some("local"));
    assert_eq!(
        settings.preferred_model("local").as_deref(),
        Some("local/b")
    );

    let _removed = SubagentTool::new(model.clone(), &ws)
        .models([SubagentModel::new("local/a", "a", model.clone())])
        .max_depth(settings.clone());
    assert_eq!(settings.model_route().as_deref(), Some("local"));
    assert_eq!(
        settings.preferred_model("local").as_deref(),
        Some("local/a")
    );

    let _tier_gone = SubagentTool::new(model, &ws).max_depth(settings.clone());
    assert_eq!(settings.model_route(), None);
    assert_eq!(settings.preferred_model("local"), None);
}

#[tokio::test]
async fn tier_route_uses_its_preferred_model_and_explicit_call_wins() {
    let default = Arc::new(ScriptedModel::new(vec![]));
    let local = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text("local")]));
    let flash = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text("flash")]));
    let settings = SubagentDepth::new(1);
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(default.clone(), &ws)
        .models([
            SubagentModel::new("local/test", "local", local.clone()),
            SubagentModel::new("flash/test", "flash", flash.clone()),
        ])
        .max_depth(settings.clone());

    assert_eq!(
        settings.preferred_model("local").as_deref(),
        Some("local/test")
    );
    assert_eq!(
        settings.preferred_model("flash").as_deref(),
        Some("flash/test")
    );
    assert!(settings.set_model_route(Some("local".into())));
    assert!(!settings.set_model_route(Some("frontier".into())));

    let routed = tool.call(json!({"task": "one"}), &ctx()).await.unwrap();
    let explicit = tool
        .call(json!({"task": "two", "model": "flash/test"}), &ctx())
        .await
        .unwrap();
    assert_eq!(routed["answer"], "local");
    assert_eq!(explicit["answer"], "flash");
    assert_eq!(default.generate_calls(), 0);
}

#[tokio::test]
async fn configured_default_model_applies_when_call_omits_model() {
    let default = Arc::new(ScriptedModel::new(vec![]));
    let flash = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text("flash")]));
    let settings = SubagentDepth::new(1);
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(default.clone(), &ws)
        .models([SubagentModel::new("flash/test", "fast", flash.clone())])
        .max_depth(settings.clone());
    assert!(settings.set_default_model(Some("flash/test".into())));

    let out = tool.call(json!({"task": "one"}), &ctx()).await.unwrap();
    assert_eq!(out["answer"], "flash");
    assert_eq!(default.generate_calls(), 0);
    assert_eq!(flash.generate_calls(), 1);
    assert!(!settings.set_default_model(Some("missing".into())));
}

#[test]
fn depth_handle_clamps_to_permitted_range() {
    let depth = SubagentDepth::new(0);
    assert_eq!(depth.get(), 1);
    assert_eq!(depth.set(99), 5);
    assert_eq!(depth.get(), 5);
    assert_eq!(depth.set(3), 3);
}

use std::sync::Mutex;

use orca_harness_core::Extension;
use orca_harness_extensions::{EventStream, HarnessEvent};
use orca_harness_tools::{BackgroundStats, SubagentSpawn};

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
