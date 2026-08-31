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
    let settings = SubagentDepth::new(2);
    let tool = SubagentTool::new(default.clone(), &ws)
        .models([SubagentModel::new("flash/test", "fast", flash.clone())
            .identity("openrouter", "vendor/flash")])
        .max_depth(settings.clone())
        .spawn_extensions(Arc::new(move |spawn| {
            recorded.lock().unwrap().push(spawn.clone());
            Vec::new()
        }));
    assert!(settings.set_default_model(Some("flash/test".into())));

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

#[test]
fn model_chosen_routes_survive_catalog_changes() {
    for route in [AUTO_SUBAGENT_ROUTE, PREFERENCE_SUBAGENT_ROUTE] {
        let settings = SubagentDepth::new(1);
        assert!(settings.set_model_route(Some(route.into())));

        let model = Arc::new(ScriptedModel::new(vec![]));
        let (ws, _dir) = temp_ws();
        let _tool = SubagentTool::new(model, &ws).max_depth(settings.clone());

        assert_eq!(settings.model_route().as_deref(), Some(route));
        assert_eq!(settings.default_model(), None);
    }
}

#[tokio::test]
async fn preference_route_only_accepts_saved_preferred_models() {
    let default = Arc::new(ScriptedModel::new(vec![]));
    let local_a = Arc::new(ScriptedModel::new(vec![]));
    let local_b = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text("local b")]));
    let flash_a = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text("flash a")]));
    let settings = SubagentDepth::new(1);
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(default.clone(), &ws)
        .models([
            SubagentModel::new("local/a", "local a", local_a.clone()),
            SubagentModel::new("local/b", "local b", local_b.clone()),
            SubagentModel::new("flash/a", "flash a", flash_a.clone()),
        ])
        .max_depth(settings.clone());
    assert!(settings.set_preferred_model("local", "local/b".into()));
    assert!(settings.set_model_route(Some(PREFERENCE_SUBAGENT_ROUTE.into())));

    let schema = tool.schema();
    assert_eq!(
        schema.parameters["properties"]["model"]["enum"],
        json!(["local/b", "flash/a"])
    );
    assert_eq!(schema.parameters["required"], json!(["task", "model"]));
    assert!(schema.description.contains("route is `preference`"));

    let local = tool
        .call(json!({"task": "one", "model": "local/b"}), &ctx())
        .await
        .unwrap();
    let flash = tool
        .call(json!({"task": "two", "model": "flash/a"}), &ctx())
        .await
        .unwrap();
    let omitted = tool
        .call(json!({"task": "three"}), &ctx())
        .await
        .unwrap_err()
        .to_string();
    let non_preferred = tool
        .call(json!({"task": "four", "model": "local/a"}), &ctx())
        .await
        .unwrap_err()
        .to_string();

    assert_eq!(local["answer"], "local b");
    assert_eq!(flash["answer"], "flash a");
    assert!(omitted.contains("requires one saved preferred `model`"));
    assert!(non_preferred.contains("not one of the user's saved preferred models"));
    assert_eq!(default.generate_calls(), 0);
    assert_eq!(local_a.generate_calls(), 0);
    assert_eq!(local_b.generate_calls(), 1);
    assert_eq!(flash_a.generate_calls(), 1);
}

#[tokio::test]
async fn tier_route_enforces_its_preferred_model() {
    let default = Arc::new(ScriptedModel::new(vec![]));
    let local = Arc::new(ScriptedModel::new(vec![
        ModelResponse::final_text("local default"),
        ModelResponse::final_text("local explicit"),
    ]));
    let flash = Arc::new(ScriptedModel::new(vec![]));
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

    let schema = tool.schema();
    assert_eq!(
        schema.parameters["properties"]["model"]["enum"],
        json!(["local/test"])
    );
    assert!(schema.description.contains("locked to `local/test`"));

    let routed = tool.call(json!({"task": "one"}), &ctx()).await.unwrap();
    let explicit = tool
        .call(json!({"task": "two", "model": "local/test"}), &ctx())
        .await
        .unwrap();
    let err = tool
        .call(json!({"task": "three", "model": "flash/test"}), &ctx())
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(routed["answer"], "local default");
    assert_eq!(explicit["answer"], "local explicit");
    assert!(err.contains("conflicts with the user's preferred model `local/test`"));
    assert_eq!(default.generate_calls(), 0);
    assert_eq!(flash.generate_calls(), 0);
    assert_eq!(local.generate_calls(), 2);
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
