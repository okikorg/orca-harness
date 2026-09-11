//! `Agent::run` is the one-shot convenience: each call opens and drops its
//! own ephemeral session, so calls are isolated from each other and may
//! overlap, unlike `Session::run` which rejects overlap with `BusySession`.

use orca_harness_core::testing::ScriptedModel;
use orca_harness_core::{Message, ModelResponse};
use orca_harness_sdk::{Harness, RunRequest, SdkError};

fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "orca-sdk-test-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn final_answer(text: &str) -> ModelResponse {
    ModelResponse::Final {
        text: text.into(),
        usage: None,
    }
}

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
    let model = ScriptedModel::new(vec![final_answer("first"), final_answer("second")]);
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
async fn agent_run_returns_model_errors() {
    let root = temp_dir("one-shot-model-error");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    // An empty script fails on the first model call.
    let model = ScriptedModel::new(Vec::new());
    let agent = harness.agent(model).build().unwrap();

    let result = agent.run("hello").await;
    assert!(matches!(result, Err(SdkError::Harness(_))), "{result:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn agent_run_allows_overlap() {
    let root = temp_dir("one-shot-overlap");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let model = ScriptedModel::new(vec![final_answer("done"), final_answer("done")]);
    let agent = harness.agent(model).build().unwrap();

    let (left, right) = tokio::join!(agent.run("left"), agent.run("right"));
    let left = left.unwrap();
    let right = right.unwrap();
    assert_eq!(left.text, "done");
    assert_eq!(right.text, "done");
    assert_eq!(user_texts(&left.messages), vec!["left".to_string()]);
    assert_eq!(user_texts(&right.messages), vec!["right".to_string()]);

    let _ = std::fs::remove_dir_all(&root);
}
