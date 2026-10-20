//! A network-off sandbox: the note every model call of the session and its
//! subagents carries, the line the `shell` tool adds below a network error,
//! and an unmarked sandbox, which gets neither.
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{Context, ExecOutput, Message, ModelResponse};
use orca_harness_sdk::orchestration::SubagentRequest;
use orca_harness_sdk::{Harness, SessionEnvironment, SubagentConfig, ToolPreset};
use serde_json::json;
use std::sync::Arc;

mod common;
mod environment_support;
use environment_support::{environment, MemSandbox};

const MESSAGE: &str = "Network is off on Free sandboxes until your organization has credit. \
    Add credit, or write to us on Discord (https://discord.gg/btTYzdzvqq).";
const NOTE: &str = "This sandbox has no network access. If a task needs the network, tell the \
    user: Network is off on Free sandboxes until your organization has credit. Add credit, or \
    write to us on Discord (https://discord.gg/btTYzdzvqq).";
const CURL: &str = "curl: (6) Could not resolve host: example.com";

/// A sandbox whose every command fails the way curl does without DNS.
fn offline() -> Arc<MemSandbox> {
    Arc::new(MemSandbox {
        exec_output: Some(ExecOutput {
            stdout: Vec::new(),
            stderr: CURL.as_bytes().to_vec(),
            exit_code: 6,
        }),
        ..Default::default()
    })
}

fn notes(context: &Context) -> usize {
    context
        .messages()
        .iter()
        .filter(|message| matches!(message, Message::User { content, .. } if content == NOTE))
        .count()
}

fn shell_stderr(messages: &[Message]) -> String {
    messages
        .iter()
        .find_map(|message| match message {
            Message::Tool { results } => results
                .iter()
                .find(|result| result.tool_name == "shell")
                .map(|result| result.output["stderr"].as_str().unwrap().to_owned()),
            _ => None,
        })
        .unwrap()
}

/// One shell call and an answer, a second run's answer, then a child's
/// answer: the same script with and without the mark.
async fn run(mark: bool) -> (Vec<Context>, String) {
    let root = common::temp_dir("network-off");
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: None,
            usage: None,
            calls: vec![call(
                "fetch",
                "shell",
                json!({"command":"curl https://example.com"}),
            )],
        },
        ModelResponse::final_text("first"),
        ModelResponse::final_text("second"),
        ModelResponse::final_text("child"),
    ]));
    let agent = harness
        .agent(model.clone())
        .tools(ToolPreset::Coding)
        .subagents(SubagentConfig::default())
        .build()
        .unwrap();
    let mut env = environment("offline", &offline(), &root);
    if mark {
        env = env.network_off(MESSAGE).unwrap();
    }
    let session = agent.new_session().environment(env).open().unwrap();
    session.run("fetch it").await.unwrap();
    session.run("and again").await.unwrap();
    session
        .subagents()
        .unwrap()
        .run(SubagentRequest::new("check"), None, None)
        .await
        .unwrap();
    let stderr = shell_stderr(&session.messages().await);
    std::fs::remove_dir_all(root).unwrap();
    (model.observed_contexts(), stderr)
}

#[tokio::test]
async fn network_off_sandbox_tells_the_model_and_the_shell_output_why() {
    let (contexts, stderr) = run(true).await;
    assert_eq!(contexts.len(), 4);
    for context in &contexts {
        assert_eq!(notes(context), 1, "{context:?}");
    }
    assert_eq!(stderr, format!("{CURL}\n[Orca] {MESSAGE}\n"));
}

#[tokio::test]
async fn unmarked_sandbox_gets_no_note_and_no_line() {
    let (contexts, stderr) = run(false).await;
    assert_eq!(contexts.len(), 4);
    assert!(contexts.iter().all(|context| notes(context) == 0));
    assert_eq!(stderr, CURL);
}

#[test]
fn only_a_sandbox_with_a_message_is_marked_and_the_binding_ignores_the_mark() {
    assert!(SessionEnvironment::Local.network_off(MESSAGE).is_err());
    assert!(SessionEnvironment::Disabled.network_off(MESSAGE).is_err());
    let root = common::temp_dir("network-off-binding");
    let sandbox = Arc::new(MemSandbox::default());
    assert!(environment("blank", &sandbox, &root)
        .network_off(" ")
        .is_err());
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let agent = harness
        .agent(ScriptedModel::new(vec![]))
        .tools(ToolPreset::Coding)
        .build()
        .unwrap();
    let marked = environment("same", &sandbox, &root)
        .network_off(MESSAGE)
        .unwrap();
    let session = agent
        .new_session()
        .persistent()
        .environment(marked.clone())
        .open()
        .unwrap();
    let id = session.id().unwrap();
    agent
        .resume_session_with_environment(&id, environment("same", &sandbox, &root))
        .unwrap();
    agent.resume_session_with_environment(&id, marked).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
