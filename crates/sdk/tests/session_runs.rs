//! Context import, continuation, and caller-owned cancellation: imported
//! histories persist and resume, the agent's system prompt wins over an
//! imported one, malformed transcripts are rejected, a continuation adds
//! no user message while keeping limits, and a caller's parent token
//! cancels the run without the run ever cancelling the caller.

use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{CancellationToken, FnTool, HarnessError, Message, ModelResponse};
use orca_harness_sdk::{Agent, Harness, RunRequest, SdkError};
use serde_json::json;
use tokio::sync::Notify;

mod common;
use common::temp_dir;

fn user(content: &str) -> Message {
    Message::User {
        content: content.into(),
        images: Vec::new(),
    }
}

fn assistant(content: &str) -> Message {
    Message::Assistant {
        content: Some(content.into()),
        tool_calls: Vec::new(),
    }
}

fn system(content: &str) -> Message {
    Message::System {
        content: content.into(),
    }
}

/// Renders a transcript as `role:text` pairs so tests can compare shapes
/// without `Message: PartialEq`.
fn shape(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|message| match message {
            Message::System { content } => format!("system:{content}"),
            Message::User { content, .. } => format!("user:{content}"),
            Message::Assistant {
                content,
                tool_calls,
            } => format!(
                "assistant:{}:{}",
                content.clone().unwrap_or_default(),
                tool_calls.len()
            ),
            Message::Tool { results } => format!("tool:{}", results.len()),
        })
        .collect()
}

fn count_system(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|message| matches!(message, Message::System { .. }))
        .count()
}

/// A tool that sleeps far longer than any test deadline. `started` fires
/// once the tool body is running so a test can cancel mid-tool.
fn slow_tool(started: Arc<Notify>) -> FnTool {
    FnTool::new(
        "slow",
        "sleeps for a long time",
        json!({ "type": "object" }),
        move |_args, _ctx| {
            let started = started.clone();
            async move {
                started.notify_one();
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(json!({ "status": "done" }))
            }
        },
    )
}

/// An agent whose model always answers with one `slow` tool call, for as
/// many runs as `rounds`.
fn slow_agent(harness: &Harness, started: Arc<Notify>, rounds: usize) -> Agent {
    let script = (0..rounds)
        .map(|index| ModelResponse::ToolCalls {
            content: None,
            calls: vec![call(&format!("s{index}"), "slow", json!({}))],
            usage: None,
        })
        .collect();
    harness
        .agent(ScriptedModel::new(script))
        .tool(slow_tool(started))
        .build()
        .unwrap()
}

#[tokio::test]
async fn imported_context_persists_and_resumes() {
    let root = temp_dir("import-persists");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let model = ScriptedModel::new(vec![ModelResponse::final_text("final")]);
    let agent = harness.agent(model).system_prompt("agent").build().unwrap();

    let session = agent
        .new_session()
        .persistent()
        .context([user("earlier"), assistant("noted")])
        .open()
        .unwrap();
    let id = session.id().unwrap();
    let result = session.run("new").await.unwrap();
    assert_eq!(result.text, "final");

    let expected = vec![
        "system:agent",
        "user:earlier",
        "assistant:noted:0",
        "user:new",
        "assistant:final:0",
    ];
    assert_eq!(shape(&session.messages().await), expected);
    drop(session);

    let resumed = agent.resume_session(&id).unwrap();
    assert_eq!(shape(&resumed.messages().await), expected);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn agent_system_prompt_replaces_imported_system() {
    let root = temp_dir("import-system-precedence");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(ScriptedModel::new(Vec::new()))
        .system_prompt("agent")
        .build()
        .unwrap();

    let session = agent
        .new_session()
        .context([system("imported"), user("x")])
        .open()
        .unwrap();
    let messages = session.messages().await;
    assert_eq!(shape(&messages), vec!["system:agent", "user:x"]);
    assert_eq!(count_system(&messages), 1);

    // No imported system message: the agent prompt is prepended.
    let session = agent.new_session().context([user("x")]).open().unwrap();
    assert_eq!(
        shape(&session.messages().await),
        vec!["system:agent", "user:x"]
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn imported_system_kept_when_agent_has_none() {
    let root = temp_dir("import-system-kept");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(ScriptedModel::new(Vec::new()))
        .build()
        .unwrap();

    let session = agent
        .new_session()
        .context([system("imported"), user("x")])
        .open()
        .unwrap();
    let messages = session.messages().await;
    assert_eq!(shape(&messages), vec!["system:imported", "user:x"]);
    assert_eq!(count_system(&messages), 1);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn import_rejects_malformed_transcripts() {
    let root = temp_dir("import-malformed");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(ScriptedModel::new(Vec::new()))
        .build()
        .unwrap();
    let tool_call = call("c1", "add", json!({}));
    let tool_result = orca_harness_core::ToolResult::ok(&tool_call, json!({}));
    let other_result = orca_harness_core::ToolResult::ok(&call("c9", "add", json!({})), json!({}));

    // (a) a system message that is not first.
    let cases: Vec<(&str, Vec<Message>)> = vec![
        ("system not first", vec![user("x"), system("late")]),
        (
            "two system messages",
            vec![system("one"), system("two"), user("x")],
        ),
        // (b) a tool message without a preceding tool-calling assistant, or
        // whose result ids do not match the calls.
        (
            "tool after user",
            vec![
                user("x"),
                Message::Tool {
                    results: vec![tool_result.clone()],
                },
            ],
        ),
        (
            "tool results mismatch calls",
            vec![
                user("x"),
                Message::Assistant {
                    content: None,
                    tool_calls: vec![tool_call.clone()],
                },
                Message::Tool {
                    results: vec![other_result],
                },
            ],
        ),
        // (c) an unanswered tool call.
        (
            "unanswered tool call",
            vec![
                user("x"),
                Message::Assistant {
                    content: None,
                    tool_calls: vec![tool_call.clone()],
                },
                user("y"),
            ],
        ),
        (
            "trailing unanswered tool call",
            vec![
                user("x"),
                Message::Assistant {
                    content: None,
                    tool_calls: vec![tool_call.clone()],
                },
            ],
        ),
    ];
    for (label, messages) in cases {
        let result = agent.new_session().context(messages).open();
        assert!(
            matches!(result, Err(SdkError::InvalidContext(_))),
            "{label} must be rejected: {:?}",
            result.err()
        );
    }

    // A well-formed tool round is accepted.
    let ok = agent
        .new_session()
        .context([
            user("x"),
            Message::Assistant {
                content: None,
                tool_calls: vec![tool_call],
            },
            Message::Tool {
                results: vec![tool_result],
            },
            assistant("done"),
        ])
        .open();
    assert!(ok.is_ok(), "{:?}", ok.err());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn continuation_adds_no_user_message() {
    let root = temp_dir("continuation-no-user");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let model = ScriptedModel::new(vec![
        ModelResponse::final_text("first"),
        ModelResponse::final_text("second"),
    ]);
    let agent = harness.agent(model).system_prompt("agent").build().unwrap();

    let session = agent.new_session().open().unwrap();
    session.run("hello").await.unwrap();
    let before = shape(&session.messages().await);

    let result = session
        .continue_run(RunRequest::continuation())
        .await
        .unwrap();
    assert_eq!(result.text, "second");
    let after = shape(&session.messages().await);
    assert_eq!(after.len(), before.len() + 1, "{after:?}");
    assert_eq!(&after[..before.len()], &before[..]);
    assert_eq!(after.last().unwrap(), "assistant:second:0");

    // `run` honors a continuation request too.
    let model = ScriptedModel::new(vec![
        ModelResponse::final_text("first"),
        ModelResponse::final_text("second"),
    ]);
    let agent = harness.agent(model).build().unwrap();
    let session = agent.new_session().open().unwrap();
    session.run("hello").await.unwrap();
    session.run(RunRequest::continuation()).await.unwrap();
    assert_eq!(
        shape(&session.messages().await),
        vec!["user:hello", "assistant:first:0", "assistant:second:0"]
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn continuation_on_empty_session_is_rejected() {
    let root = temp_dir("continuation-empty");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(ScriptedModel::new(vec![ModelResponse::final_text("never")]))
        .system_prompt("agent")
        .build()
        .unwrap();

    let session = agent.new_session().open().unwrap();
    let result = session.continue_run(RunRequest::continuation()).await;
    assert!(
        matches!(result, Err(SdkError::InvalidContext(_))),
        "{result:?}"
    );
    assert_eq!(shape(&session.messages().await), vec!["system:agent"]);

    // A background continuation fails from `finish`, before any model call.
    let handle = session.start(RunRequest::continuation()).unwrap();
    let result = handle.finish().await;
    assert!(
        matches!(result, Err(SdkError::InvalidContext(_))),
        "{result:?}"
    );
    assert_eq!(shape(&session.messages().await), vec!["system:agent"]);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn continuation_rejects_prompt_and_images() {
    let root = temp_dir("continuation-payload");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let model = ScriptedModel::new(vec![ModelResponse::final_text("first")]);
    let agent = harness.agent(model).build().unwrap();
    let session = agent.new_session().open().unwrap();
    session.run("hello").await.unwrap();
    let before = shape(&session.messages().await);

    // A prompt on a continuation would be silently dropped; reject it.
    let result = session.continue_run(RunRequest::new("hello again")).await;
    assert!(matches!(result, Err(SdkError::Config(_))), "{result:?}");
    let mut with_prompt = RunRequest::continuation();
    with_prompt.prompt = "late".into();
    let result = session.run(with_prompt).await;
    assert!(matches!(result, Err(SdkError::Config(_))), "{result:?}");

    // Images have no message to attach to on a continuation.
    let with_image = RunRequest::continuation().image(orca_harness_core::Image {
        media_type: "image/png".into(),
        data: "AAAA".into(),
    });
    let result = session.continue_run(with_image).await;
    assert!(matches!(result, Err(SdkError::Config(_))), "{result:?}");

    // `start` rejects synchronously, and nothing was appended.
    let mut with_prompt = RunRequest::continuation();
    with_prompt.prompt = "late".into();
    let result = session.start(with_prompt).err();
    assert!(matches!(result, Some(SdkError::Config(_))), "{result:?}");
    assert_eq!(shape(&session.messages().await), before);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn continuation_honors_deadline() {
    let root = temp_dir("continuation-deadline");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let started = Arc::new(Notify::new());
    let model = ScriptedModel::new(vec![
        ModelResponse::final_text("first"),
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call("s1", "slow", json!({}))],
            usage: None,
        },
    ]);
    let agent = harness
        .agent(model)
        .tool(slow_tool(started))
        .build()
        .unwrap();

    let session = agent.new_session().open().unwrap();
    session.run("hello").await.unwrap();
    let result = session
        .continue_run(RunRequest::continuation().deadline(Duration::from_millis(50)))
        .await;
    assert!(
        matches!(
            result,
            Err(SdkError::Harness(HarnessError::DeadlineExceeded))
        ),
        "{result:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn caller_token_cancels_run_but_run_does_not_cancel_caller() {
    let root = temp_dir("caller-cancellation");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let started = Arc::new(Notify::new());
    let agent = slow_agent(&harness, started.clone(), 3);
    let session = agent.new_session().open().unwrap();

    // Cancelling the caller's parent token cancels the run.
    let parent = CancellationToken::new();
    let handle = session
        .start(RunRequest::new("go").cancellation(parent.clone()))
        .unwrap();
    started.notified().await;
    parent.cancel();
    assert!(handle.cancellation_token().is_cancelled());
    let result = handle.finish().await;
    assert!(
        matches!(result, Err(SdkError::Harness(HarnessError::Cancelled))),
        "{result:?}"
    );

    // Cancelling the run's own token leaves the caller's token alone.
    let parent = CancellationToken::new();
    let handle = session
        .start(RunRequest::new("go").cancellation(parent.clone()))
        .unwrap();
    started.notified().await;
    handle.cancellation_token().cancel();
    let result = handle.finish().await;
    assert!(
        matches!(result, Err(SdkError::Harness(HarnessError::Cancelled))),
        "{result:?}"
    );
    assert!(!parent.is_cancelled(), "run must not cancel the caller");

    // Dropping the handle cancels the run, not the caller.
    let parent = CancellationToken::new();
    let handle = session
        .start(RunRequest::new("go").cancellation(parent.clone()))
        .unwrap();
    started.notified().await;
    let token = handle.cancellation_token();
    drop(handle);
    assert!(token.is_cancelled());
    assert!(
        !parent.is_cancelled(),
        "dropping the handle must not cancel the caller"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn cancelled_persistent_run_syncs_partial_transcript() {
    let root = temp_dir("cancelled-persists");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let started = Arc::new(Notify::new());
    let agent = slow_agent(&harness, started.clone(), 1);
    let session = agent.new_session().persistent().open().unwrap();
    let id = session.id().unwrap();

    let parent = CancellationToken::new();
    let handle = session
        .start(RunRequest::new("go").cancellation(parent.clone()))
        .unwrap();
    started.notified().await;
    parent.cancel();
    let result = handle.finish().await;
    assert!(
        matches!(result, Err(SdkError::Harness(HarnessError::Cancelled))),
        "{result:?}"
    );
    drop(session);

    let resumed = agent.resume_session(&id).unwrap();
    let messages = resumed.messages().await;
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, Message::User { content, .. } if content == "go")),
        "user message must be on disk after cancellation: {:?}",
        shape(&messages)
    );

    let _ = std::fs::remove_dir_all(&root);
}
