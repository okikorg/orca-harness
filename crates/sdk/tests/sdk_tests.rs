use std::sync::Arc;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{FnTool, ModelResponse, ToolCall, Usage};
use orca_harness_extensions::{CompactConfig, HarnessEvent, PolicyOutcome, ToolPolicy};
use orca_harness_sdk::{Compaction, Harness, MemoryConfig, RunRequest, SkillDestination, Skills};
use serde_json::json;

mod common;
use common::temp_dir;

#[tokio::test]
async fn run_text_events_and_custom_tool() {
    let root = temp_dir("run-text-events-tool");
    let harness = Harness::builder().workspace(&root).build().unwrap();

    let custom_tool = FnTool::new(
        "add",
        "adds two numbers",
        json!({
            "type": "object",
            "properties": {
                "a": {"type": "number"},
                "b": {"type": "number"}
            },
            "required": ["a", "b"]
        }),
        |args, _ctx| async move {
            let a = args["a"].as_f64().unwrap_or(0.0);
            let b = args["b"].as_f64().unwrap_or(0.0);
            Ok(json!({ "sum": a + b }))
        },
    );

    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call("c1", "add", json!({"a": 19, "b": 23}))],
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
                reasoning_tokens: None,
            }),
        },
        ModelResponse::Final {
            text: "The sum is 42".into(),
            usage: Some(Usage {
                input_tokens: 20,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
                reasoning_tokens: None,
            }),
        },
    ]);

    let agent = harness
        .agent(model)
        .name("test-agent")
        .system_prompt("You are a helpful math agent.")
        .tool(custom_tool)
        .events(true)
        .usage(true)
        .build()
        .unwrap();

    let session = agent.new_session().ephemeral().open().unwrap();

    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let events_clone = events.clone();

    let req = RunRequest::new("Add 19 and 23").on_event(move |ev| {
        events_clone.lock().unwrap().push(ev);
    });

    let res = session.run(req).await.unwrap();
    assert_eq!(res.text, "The sum is 42");
    assert_eq!(res.metered_steps, 2);
    assert_eq!(res.usage.input_tokens, 30);
    assert_eq!(res.usage.output_tokens, 15);

    let recorded_events = events.lock().unwrap().clone();
    assert!(!recorded_events.is_empty());
    let has_tool_event = recorded_events.iter().any(|ev| match ev {
        HarnessEvent::ToolCall { tool_name, .. } => tool_name == "add",
        _ => false,
    });
    assert!(has_tool_event, "tool call event expected");

    let messages = session.messages().await;
    // Messages in context: system, user, assistant (tool calls), tool result, assistant (final)
    assert_eq!(messages.len(), 5);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn overlapping_runs_are_rejected_until_handle_finishes() {
    let root = temp_dir("busy-session");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let slow_tool = FnTool::new(
        "slow",
        "wait",
        json!({"type":"object"}),
        |_args, ctx| async move {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => Ok(json!({"ok": true})),
                _ = ctx.cancellation.cancelled() => Ok(json!({"cancelled": true})),
            }
        },
    );
    let model = ScriptedModel::tool_round(vec![call("slow-1", "slow", json!({}))], "done");
    let agent = harness.agent(model).tool(slow_tool).build().unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();

    let handle = session.start("first").unwrap();
    assert!(matches!(
        session.start("second"),
        Err(orca_harness_sdk::SdkError::BusySession)
    ));
    assert!(matches!(
        session.clear().await,
        Err(orca_harness_sdk::SdkError::BusySession)
    ));
    assert_eq!(handle.finish().await.unwrap().text, "done");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn custom_tool_overrides_same_named_preset_tool() {
    let root = temp_dir("tool-precedence");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let custom = FnTool::new(
        "read_file",
        "custom reader",
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        |_args, _ctx| async { Ok(json!({"custom": true})) },
    );
    let model = ScriptedModel::tool_round(
        vec![call("read-1", "read_file", json!({"path":"missing"}))],
        "custom won",
    );
    let agent = harness
        .agent(model)
        .tools(orca_harness_sdk::ToolPreset::ReadOnly)
        .tool(custom)
        .build()
        .unwrap();
    let result = agent
        .new_session()
        .ephemeral()
        .open()
        .unwrap()
        .run("read")
        .await
        .unwrap();
    assert!(format!("{:?}", result.messages).contains("custom"));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn persistent_recovery_store_survives_resume_and_fork() {
    let root = temp_dir("recovery-persistence");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let large = "x".repeat(256);
    let large_tool = FnTool::new(
        "large",
        "large output",
        json!({"type":"object"}),
        move |_args, _ctx| {
            let large = large.clone();
            async move { Ok(json!({"value": large})) }
        },
    );
    let first_model =
        ScriptedModel::tool_round(vec![call("large-1", "large", json!({}))], "stored");
    let first_agent = harness
        .agent(first_model)
        .tool(large_tool)
        .truncation(orca_harness_sdk::TruncationConfig {
            max_string_chars: 32,
            ..Default::default()
        })
        .build()
        .unwrap();
    let session = first_agent.new_session().persistent().open().unwrap();
    let id = session.id().unwrap();
    session.run("make output").await.unwrap();

    let resumed_model = ScriptedModel::tool_round(
        vec![call(
            "read-1",
            "read_tool_result",
            json!({"callId":"large-1"}),
        )],
        "recovered",
    );
    let resumed_agent = harness.agent(resumed_model).build().unwrap();
    let resumed = resumed_agent.resume_session(&id).unwrap();
    assert_eq!(resumed.run("recover").await.unwrap().text, "recovered");
    let fork = resumed.fork().await.unwrap();
    assert_ne!(fork.id(), resumed.id());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn run_handle_async_streaming_events() {
    let root = temp_dir("run-handle-events");
    let harness = Harness::builder().workspace(&root).build().unwrap();

    let model = ScriptedModel::new(vec![ModelResponse::final_text("hello async")]);
    let agent = harness.agent(model).events(true).build().unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();

    let mut handle = session.start("hi").unwrap();
    let mut event_count = 0;
    while let Some(_event) = handle.events().recv().await {
        event_count += 1;
    }

    let result = handle.finish().await.unwrap();
    assert_eq!(result.text, "hello async");
    assert!(event_count > 0);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn persistent_session_create_list_resume_and_multi_turn() {
    let root = temp_dir("session-persist");
    let harness = Harness::builder().workspace(&root).build().unwrap();

    let model1 = ScriptedModel::new(vec![ModelResponse::final_text("Turn 1 response")]);
    let agent1 = harness
        .agent(model1)
        .name("persistent-agent")
        .system_prompt("Be persistent.")
        .build()
        .unwrap();

    let session1 = agent1.new_session().persistent().open().unwrap();
    let session_id = session1.id().expect("persistent session has id");
    assert!(session1.path().is_some());

    let res1 = session1.run("Hello turn 1").await.unwrap();
    assert_eq!(res1.text, "Turn 1 response");

    // List sessions via harness
    let listed = harness.sessions().list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].meta.id, session_id);
    assert_eq!(
        listed[0].meta.workspace,
        harness.workspace().root().display().to_string()
    );

    // Resume session with agent2
    let model2 = ScriptedModel::new(vec![ModelResponse::final_text("Turn 2 response")]);
    let agent2 = harness
        .agent(model2)
        .name("persistent-agent")
        .system_prompt("Be persistent.")
        .build()
        .unwrap();

    let session2 = agent2.resume_session(&session_id).unwrap();
    assert_eq!(session2.id().as_deref(), Some(session_id.as_str()));

    let res2 = session2.run("Hello turn 2").await.unwrap();
    assert_eq!(res2.text, "Turn 2 response");

    let messages = session2.messages().await;
    // Messages: system, user1, assistant1, user2 (inside context run)
    assert!(messages.len() >= 4);

    // Fork session
    let model3 = ScriptedModel::new(vec![ModelResponse::final_text("Fork turn response")]);
    let _agent3 = harness.agent(model3).build().unwrap();
    let forked_session = session2.fork().await.unwrap();
    assert_ne!(forked_session.id(), session2.id());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn direct_memory_crud_and_automatic_recall() {
    let root = temp_dir("memory-crud");
    let harness = Harness::builder().workspace(&root).build().unwrap();

    let memory = harness.memory().unwrap();

    // Create
    let record1 = memory
        .save(
            "The user's favorite language is Rust",
            "fact",
            false,
            "test_call_1",
        )
        .unwrap();
    assert_eq!(record1.content, "The user's favorite language is Rust");
    assert_eq!(record1.kind, "fact");

    let record2 = memory
        .save(
            "Deployment target is AWS us-east-1",
            "preference",
            false,
            "test_call_2",
        )
        .unwrap();

    // Read / List
    let all = memory.list(10).unwrap();
    assert_eq!(all.len(), 2);

    // Search
    let search_res = memory.search("Rust", 5).unwrap();
    assert!(!search_res.is_empty());
    assert_eq!(search_res[0].id, record1.id);

    // Update
    let updated = memory
        .update(
            &record2.id,
            "Deployment target is AWS eu-central-1",
            Some("preference"),
        )
        .unwrap();
    assert!(updated.is_some());
    let updated_rec = updated.unwrap();
    assert_eq!(updated_rec.content, "Deployment target is AWS eu-central-1");
    assert_eq!(updated_rec.kind, "preference");

    // Forget / Delete
    let forgotten = memory.forget(&record2.id).unwrap();
    assert!(forgotten);
    let after_forget = memory.list(10).unwrap();
    assert_eq!(after_forget.len(), 1);
    assert_eq!(after_forget[0].id, record1.id);

    // Automatic recall test in agent run
    let model = ScriptedModel::new(vec![ModelResponse::final_text("Rust is great!")]);
    let agent = harness
        .agent(model)
        .memory(MemoryConfig::new(memory).automatic_recall(true))
        .build()
        .unwrap();

    let session = agent.new_session().ephemeral().open().unwrap();
    let res = session.run("What language is favorite?").await.unwrap();
    assert_eq!(res.text, "Rust is great!");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn skills_discovery_enable_and_disable() {
    let root = temp_dir("skills-test");
    let harness = Harness::builder().workspace(&root).build().unwrap();

    // Pass home: None to isolate from real user ~/.claude/skills or ~/.agents/skills
    let skills = Skills::new(
        harness.workspace().root(),
        Some(harness.state_dir().to_path_buf()),
        None,
    );

    // Scaffold a skill into the workspace destination
    let created_dir = skills
        .scaffold("code_helper", SkillDestination::Workspace)
        .unwrap();
    assert!(created_dir.exists());

    // Discovered skills
    let discovered = skills.reload();
    let found = discovered.skills.iter().find(|s| s.name == "code_helper");
    assert!(found.is_some(), "scaffolded skill should be discovered");
    assert_eq!(discovered.skills.len(), 1);

    // Check catalog
    let catalog = skills.catalog();
    assert_eq!(catalog.skills.len(), 1);

    // Tool should be available when skill is enabled
    assert!(skills.tool().is_some());

    // Disable skill
    skills.disable("code_helper");
    // With all skills disabled, tool() returns None
    assert!(skills.tool().is_none());

    // Re-enable skill
    skills.enable("code_helper");
    assert!(skills.tool().is_some());

    // Uninstall skill
    let uninstalled = skills.uninstall("code_helper").unwrap();
    assert!(uninstalled);

    // After uninstallation, reload to update discovered cache
    let reloaded = skills.reload();
    assert!(reloaded.skills.is_empty());
    assert!(skills.tool().is_none());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn manual_compaction_of_session_context() {
    let root = temp_dir("manual-compaction");
    let harness = Harness::builder().workspace(&root).build().unwrap();

    let model = ScriptedModel::new(vec![
        ModelResponse::final_text("First answer"),
        ModelResponse::final_text("Second answer"),
        ModelResponse::final_text("Third answer"),
    ]);

    let agent = harness
        .agent(model)
        .name("compaction-agent")
        .compaction(Compaction::Manual)
        .build()
        .unwrap();

    let session = agent.new_session().persistent().open().unwrap();

    session
        .run("Long question 1 with lots of detail")
        .await
        .unwrap();
    session
        .run("Long question 2 with more detail")
        .await
        .unwrap();
    session
        .run("Long question 3 asking for summary")
        .await
        .unwrap();

    let messages_before = session.messages().await;
    assert!(messages_before.len() >= 6);

    // Compact the session with a tight tail budget
    let report = session
        .compact(CompactConfig {
            tail_budget_tokens: 20,
        })
        .await
        .unwrap();

    assert!(report.messages_after < report.messages_before);
    let messages_after = session.messages().await;
    assert!(messages_after.len() < messages_before.len());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn custom_extension_and_policy() {
    let root = temp_dir("extension-policy");
    let harness = Harness::builder().workspace(&root).build().unwrap();

    let blocked_tool = FnTool::new(
        "blocked_command",
        "should be denied by policy",
        json!({"type": "object"}),
        |_i, _c| async move { Ok(json!({"result": "unreachable"})) },
    );
    let allowed_tool = FnTool::new(
        "allowed_command",
        "should be allowed by policy",
        json!({"type": "object"}),
        |_i, _c| async move { Ok(json!({"result": "success"})) },
    );

    let policy = ToolPolicy::new().rule(|call: &ToolCall| {
        if call.name == "blocked_command" {
            PolicyOutcome::Deny("command blocked by security policy".into())
        } else {
            PolicyOutcome::Allow
        }
    });

    let model = ScriptedModel::tool_round(
        vec![
            call("c1", "blocked_command", json!({})),
            call("c2", "allowed_command", json!({})),
        ],
        "Finished execution",
    );

    let agent = harness
        .agent(model)
        .tool(blocked_tool)
        .tool(allowed_tool)
        .policy(policy)
        .build()
        .unwrap();

    let session = agent.new_session().ephemeral().open().unwrap();
    let res = session.run("Run both commands").await.unwrap();
    assert_eq!(res.text, "Finished execution");

    let messages = session.messages().await;
    // Check that blocked_command got an error result containing the policy denial message
    let found_denial = messages.iter().any(|msg| {
        serde_json::to_string(msg)
            .unwrap()
            .contains("command blocked by security policy")
    });
    assert!(found_denial, "expected policy denial error in tool results");

    let _ = std::fs::remove_dir_all(&root);
}

#[path = "sdk_tests/mcp.rs"]
mod mcp;
