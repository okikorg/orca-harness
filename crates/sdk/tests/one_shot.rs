//! `Agent::run` is the one-shot convenience: each call opens and drops its
//! own ephemeral session, so calls are isolated from each other and may
//! overlap, unlike `Session::run` which rejects overlap with `BusySession`.

use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{FnTool, HarnessError, Message, ModelResponse};
use orca_harness_sdk::{Harness, RunRequest, SdkError};
use serde_json::json;
use tokio::sync::Barrier;

mod common;
use common::temp_dir;

fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::User { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn agent_run_is_one_shot_and_isolated() {
    let root = temp_dir("one-shot-isolated");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let model = ScriptedModel::new(vec![
        ModelResponse::final_text("first"),
        ModelResponse::final_text("second"),
    ]);
    let agent = harness
        .agent(model)
        .system_prompt("You are terse.")
        .build()
        .unwrap();

    let first = agent.run("hello").await.unwrap();
    assert_eq!(first.text, "first");
    assert_eq!(first.messages.len(), 3, "system, user, assistant");

    let second = agent.run(RunRequest::new("again")).await.unwrap();
    assert_eq!(second.text, "second");
    assert_eq!(
        second.messages.len(),
        3,
        "second call must not carry the first conversation"
    );
    assert_eq!(user_texts(&second.messages), vec!["again".to_string()]);
    assert!(matches!(
        &second.messages[0],
        Message::System { content, .. } if content == "You are terse."
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn agent_run_propagates_run_errors() {
    let root = temp_dir("one-shot-model-error");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    // An empty script fails on the first model call.
    let model = ScriptedModel::new(Vec::new());
    let agent = harness.agent(model).build().unwrap();

    let result = agent.run("hello").await;
    assert!(
        matches!(result, Err(SdkError::Harness(HarnessError::Model(_)))),
        "{result:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Both runs must be in flight at the same time for the barrier to release,
/// so a regression to a shared busy guard or serialized execution shows up
/// as a timeout rather than a pass.
#[tokio::test]
async fn agent_run_allows_overlap() {
    let root = temp_dir("one-shot-overlap");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let rendezvous = FnTool::new(
        "rendezvous",
        "waits for the other run",
        json!({"type": "object"}),
        move |_args, _ctx| {
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                Ok(json!({"met": true}))
            }
        },
    );
    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call("r1", "rendezvous", json!({}))],
            usage: None,
        },
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call("r2", "rendezvous", json!({}))],
            usage: None,
        },
        ModelResponse::final_text("done"),
        ModelResponse::final_text("done"),
    ]);
    let agent = harness.agent(model).tool(rendezvous).build().unwrap();

    let (left, right) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(agent.run("left"), agent.run("right"))
    })
    .await
    .expect("both runs must reach the rendezvous concurrently");
    let left = left.unwrap();
    let right = right.unwrap();
    assert_eq!(left.text, "done");
    assert_eq!(right.text, "done");
    assert_eq!(user_texts(&left.messages), vec!["left".to_string()]);
    assert_eq!(user_texts(&right.messages), vec!["right".to_string()]);

    let _ = std::fs::remove_dir_all(&root);
}
