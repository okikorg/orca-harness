use super::*;
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{Agent, Context, FnTool, Message, Model, ModelResponse};
use orca_harness_extensions::{EventStream, SessionFile, SessionHandler};
use serde_json::{json, Value};

fn messages(context: &Context) -> Value {
    serde_json::to_value(context.messages()).unwrap()
}

fn briefed(context: &Context) -> bool {
    context.messages().iter().any(|message| {
        matches!(message, Message::System { content } if content.contains("Orchestrate mode is on for the parent"))
    })
}

fn assert_leading_briefing(context: &Context, durable_system: Option<&str>) {
    let Message::System { content } = &context.messages()[0] else {
        panic!("request briefing was not the leading system message");
    };
    let briefing = match durable_system {
        Some(system) => content
            .strip_prefix(&format!("{system}\n\n"))
            .expect("durable system instructions were not kept first"),
        None => content.as_str(),
    };
    assert!(briefing.starts_with("Orchestrate mode is on for the parent"));
    assert!(briefing.contains("Small/basic code edits"));
    assert!(briefing.contains("delegate significant implementation and testing"));
    assert!(briefing.contains("Delegate command execution needed to verify changes"));
    assert!(briefing.contains("delegate the repair and its tests before reporting completion"));
    assert!(briefing.contains("Do not claim that you personally ran commands"));
}

// Exercise the actual kernel and append-cursor recorder, not a mock sync.
// Both request paths must leave every durable message in its original position.
#[tokio::test]
async fn session_preserves_tool_pair_and_user_on_mode_exit() {
    for streaming in [false, true] {
        let dir = std::env::temp_dir().join(orca_harness_extensions::new_session_id());
        let session = Arc::new(SessionHandler::create(&dir, "workspace", "test").unwrap());
        let mode = ModeHandle::new(Mode::Orchestrate);
        let call = call("read-1", "read_file", json!({"path": "hello.rs"}));
        let inner = Arc::new(ScriptedModel::new(vec![
            ModelResponse::tool_calls(vec![call.clone()]),
            ModelResponse::final_text("read complete"),
            ModelResponse::final_text("normal answer"),
        ]));
        let mut agent = Agent::new(OrchestrateModel::new(inner.clone(), mode.clone()))
            .extension(PlanGate::new(mode.clone(), PlanArea::new()))
            .extension_arc(session.clone())
            .tool(FnTool::new("read_file", "read", json!({}), |_, _| async {
                Ok(json!({"content": "hello"}))
            }));
        if streaming {
            agent = agent.extension(EventStream::from_fn(|_| {}));
        }
        let mut context = Context::new();
        context.push_system("existing instructions");
        context.push_user("read the file");
        agent
            .run_context(&mut context, Default::default())
            .await
            .unwrap();
        let mut expected = Context::new();
        expected.push_system("existing instructions");
        expected.push_user("read the file");
        expected.push_assistant_tool_calls(None, vec![call]);
        expected.append_tool_results(vec![orca_harness_core::ToolResult {
            call_id: "read-1".into(),
            tool_name: "read_file".into(),
            output: json!({"content": "hello"}),
            is_error: false,
        }]);
        expected.push_assistant_text("read complete");
        assert_eq!(messages(&context), messages(&expected));
        assert_eq!(
            messages(&SessionFile::load(&session.path()).unwrap().context),
            messages(&expected)
        );

        mode.set(Mode::Normal);
        context.push_user("now continue normally");
        agent
            .run_context(&mut context, Default::default())
            .await
            .unwrap();
        expected.push_user("now continue normally");
        expected.push_assistant_text("normal answer");
        assert_eq!(messages(&context), messages(&expected));
        session.sync(&context); // repeated recording must not duplicate anything
        assert_eq!(
            messages(&SessionFile::load(&session.path()).unwrap().context),
            messages(&expected)
        );
        let observed = inner.observed_contexts();
        assert_leading_briefing(&observed[0], Some("existing instructions"));
        assert_leading_briefing(&observed[1], Some("existing instructions"));
        assert_eq!(
            serde_json::to_value(&observed[0].messages()[1..]).unwrap(),
            serde_json::to_value(&expected.messages()[1..2]).unwrap()
        );
        assert!(!briefed(&observed[2]));
        assert_eq!(inner.streaming_calls(), if streaming { 3 } else { 0 });
        drop(agent);
        drop(session);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[tokio::test]
async fn request_briefing_tracks_live_modes_without_reaching_workers() {
    let mode = ModeHandle::default();
    let inner = Arc::new(ScriptedModel::new(
        (0..32).map(|_| ModelResponse::final_text("ok")).collect(),
    ));
    // Production captures this worker model before wrapping the parent.
    let worker = inner.clone();
    let parent = OrchestrateModel::new(inner.clone(), mode.clone());
    let mut context = Context::new();
    context.push_user("task");
    let original = messages(&context);
    for streaming in [false, true] {
        for other in [Mode::Normal, Mode::Plan, Mode::Auto, Mode::Yolo] {
            for next in [Mode::Orchestrate, other] {
                mode.set(next);
                if streaming {
                    parent
                        .generate_streaming(&context, &[], &|_| {})
                        .await
                        .unwrap();
                } else {
                    parent.generate(&context, &[]).await.unwrap();
                }
                let seen = inner.observed_contexts().pop().unwrap();
                assert_eq!(briefed(&seen), next == Mode::Orchestrate);
                if next == Mode::Orchestrate {
                    assert_leading_briefing(&seen, None);
                    assert_eq!(
                        serde_json::to_value(&seen.messages()[1..]).unwrap(),
                        original
                    );
                } else {
                    assert_eq!(messages(&seen), original);
                }
                if streaming {
                    worker
                        .generate_streaming(&context, &[], &|_| {})
                        .await
                        .unwrap();
                } else {
                    worker.generate(&context, &[]).await.unwrap();
                }
                assert!(!briefed(&inner.observed_contexts().pop().unwrap()));
                assert_eq!(messages(&context), original);
            }
        }
    }
    assert_eq!(inner.generate_calls(), 16);
    assert_eq!(inner.streaming_calls(), 16);
}
