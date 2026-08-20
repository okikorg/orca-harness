//! End-to-end loop behavior with a fake (scripted) LLM.

use std::time::Duration;

use serde_json::json;
use std::sync::Arc;
use tokio::time::timeout;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{Agent, FnTool, HarnessError, Limits, Message, ModelResponse};

const RUN_TIMEOUT: Duration = Duration::from_secs(10);

fn echo_tool() -> FnTool {
    FnTool::new(
        "echo",
        "echoes input",
        json!({"type": "object"}),
        |input, _ctx| async move { Ok(input) },
    )
}

#[tokio::test]
async fn final_response_ends_the_run() {
    let model = Arc::new(ScriptedModel::new(vec![ModelResponse::final_text("42")]));
    let agent = Agent::new(model.clone()).system_prompt("be brief");
    let answer = timeout(RUN_TIMEOUT, agent.run("meaning of life?"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(answer, "42");
    assert_eq!(model.generate_calls(), 1);

    // The model saw system + user.
    let seen = model.observed_contexts();
    let messages = seen[0].messages();
    assert!(matches!(&messages[0], Message::System { content } if content == "be brief"));
    assert!(matches!(&messages[1], Message::User { content } if content == "meaning of life?"));
}

#[tokio::test]
async fn tool_round_builds_the_expected_transcript() {
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("let me check".into()),
            calls: vec![call("call_0", "echo", json!({"v": 7}))],
            usage: None,
        },
        ModelResponse::final_text("done"),
    ]));
    let agent = Agent::new(model.clone()).tool(echo_tool());
    let answer = timeout(RUN_TIMEOUT, agent.run("check"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(answer, "done");
    assert_eq!(model.generate_calls(), 2);

    // Second generate call sees: user, assistant(with calls), tool results.
    let seen = model.observed_contexts();
    let messages = seen[1].messages();
    assert!(matches!(&messages[0], Message::User { .. }));
    match &messages[1] {
        Message::Assistant {
            content,
            tool_calls,
        } => {
            assert_eq!(content.as_deref(), Some("let me check"));
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].id, "call_0");
        }
        other => panic!("expected assistant tool-call message, got {other:?}"),
    }
    match &messages[2] {
        Message::Tool { results } => {
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].call_id, "call_0");
            assert_eq!(results[0].output, json!({"v": 7}));
            assert!(!results[0].is_error);
        }
        other => panic!("expected tool results message, got {other:?}"),
    }
}

#[tokio::test]
async fn step_limit_terminates_a_looping_model() {
    // A model that keeps asking for tools forever.
    let rounds = (0..10)
        .map(|i| ModelResponse::ToolCalls {
            content: None,
            calls: vec![call(&format!("call_{i}"), "echo", json!({}))],
            usage: None,
        })
        .collect();
    let model = ScriptedModel::new(rounds);
    let agent = Agent::new(model).tool(echo_tool()).limits(Limits {
        max_steps: 3,
        ..Limits::default()
    });

    let result = timeout(RUN_TIMEOUT, agent.run("loop forever"))
        .await
        .unwrap();
    assert!(
        matches!(result, Err(HarnessError::StepLimitExceeded)),
        "got: {result:?}"
    );
}

#[tokio::test]
async fn unknown_tool_feeds_error_back_to_model() {
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("call_0", "does_not_exist", json!({}))],
        "recovered",
    ));
    let agent = Agent::new(model.clone()).tool(echo_tool());
    let answer = timeout(RUN_TIMEOUT, agent.run("hallucinate"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(answer, "recovered", "unknown tool is model-recoverable");
    let seen = model.observed_contexts();
    let messages = seen[1].messages();
    match messages
        .iter()
        .rev()
        .find(|m| matches!(m, Message::Tool { .. }))
        .unwrap()
    {
        Message::Tool { results } => {
            assert!(results[0].is_error);
            assert_eq!(results[0].call_id, "call_0");
            let text = results[0].output["error"].as_str().unwrap();
            assert!(text.contains("unknown tool"), "got: {text}");
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn model_errors_are_terminal() {
    // Empty script: first generate reports a model error.
    let model = ScriptedModel::new(vec![]);
    let agent = Agent::new(model);
    let result = timeout(RUN_TIMEOUT, agent.run("no script")).await.unwrap();
    assert!(
        matches!(result, Err(HarnessError::Model(_))),
        "got: {result:?}"
    );
}

#[tokio::test]
async fn multi_round_conversation_accumulates_context() {
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call("r1_0", "echo", json!({"round": 1}))],
            usage: None,
        },
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call("r2_0", "echo", json!({"round": 2}))],
            usage: None,
        },
        ModelResponse::final_text("two rounds"),
    ]));
    let agent = Agent::new(model.clone()).tool(echo_tool());
    let answer = timeout(RUN_TIMEOUT, agent.run("go"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(answer, "two rounds");
    assert_eq!(model.generate_calls(), 3);
    // Third generate sees both prior rounds: user + 2×(assistant+tool).
    let seen = model.observed_contexts();
    assert_eq!(seen[2].messages().len(), 5);
}
