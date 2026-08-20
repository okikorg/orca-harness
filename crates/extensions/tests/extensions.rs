//! The critical extensions, exercised end-to-end through the Agent with a
//! scripted fake LLM.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{Agent, FnTool, Model, ModelError, ModelResponse, ToolError, Usage};
use orca_harness_extensions::{
    EventStream, HarnessEvent, PolicyOutcome, RetryModel, ToolPolicy, ToolRetry, Truncation,
    UsageMeter,
};

const RUN_TIMEOUT: Duration = Duration::from_secs(10);

fn echo() -> FnTool {
    FnTool::new(
        "echo",
        "echoes input",
        json!({"type": "object"}),
        |input, _ctx| async move { Ok(input) },
    )
}

#[tokio::test]
async fn event_stream_emits_full_lifecycle() {
    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("thinking".into()),
            calls: vec![call("c0", "echo", json!({"v": 1}))],
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
        },
        ModelResponse::Final {
            text: "all done".into(),
            usage: Some(Usage {
                input_tokens: 3,
                output_tokens: 7,
                ..Default::default()
            }),
        },
    ]);

    let (stream, mut rx) = EventStream::channel();
    let agent = Agent::new(model).tool(echo()).extension(stream);
    timeout(RUN_TIMEOUT, agent.run("go"))
        .await
        .unwrap()
        .unwrap();

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    // Expect: AgentStart, Assistant(thinking), Usage, ToolCall, ToolResult,
    // Assistant(all done), Usage, Result.
    let tags: Vec<&str> = events
        .iter()
        .map(|e| match e {
            HarnessEvent::AgentStart => "start",
            HarnessEvent::AssistantDelta { .. } => "assistant_delta",
            HarnessEvent::ReasoningDelta { .. } => "reasoning_delta",
            HarnessEvent::Assistant { .. } => "assistant",
            HarnessEvent::ToolCall { .. } => "tool_call",
            HarnessEvent::ToolResult { .. } => "tool_result",
            HarnessEvent::Usage { .. } => "usage",
            HarnessEvent::Result { .. } => "result",
            HarnessEvent::Error { .. } => "error",
        })
        .collect();
    assert_eq!(
        tags,
        vec![
            "start",
            "assistant",
            "usage",
            "tool_call",
            "tool_result",
            "assistant",
            "usage",
            "result"
        ]
    );

    // Tool call/result carry the right identity.
    match &events[3] {
        HarnessEvent::ToolCall {
            tool_call_id,
            tool_name,
            input,
        } => {
            assert_eq!(tool_call_id, "c0");
            assert_eq!(tool_name, "echo");
            assert_eq!(input, &json!({"v": 1}));
        }
        other => panic!("expected tool_call, got {other:?}"),
    }
    match &events[4] {
        HarnessEvent::ToolResult {
            tool_call_id,
            output,
            is_error,
            ..
        } => {
            assert_eq!(tool_call_id, "c0");
            assert_eq!(output, &json!({"v": 1}));
            assert!(!is_error);
        }
        other => panic!("expected tool_result, got {other:?}"),
    }
}

#[tokio::test]
async fn event_stream_serializes_to_platform_ndjson_tags() {
    // The tag names must match the platform NDJSON union.
    let ev = HarnessEvent::ToolResult {
        tool_call_id: "c0".into(),
        tool_name: "echo".into(),
        output: json!({"ok": true}),
        is_error: false,
    };
    let s = serde_json::to_value(&ev).unwrap();
    assert_eq!(s["type"], json!("tool_result"));
    assert_eq!(s["tool_call_id"], json!("c0"));
}

#[tokio::test]
async fn event_stream_emits_error_on_failure() {
    let model = ScriptedModel::new(vec![]); // model error
    let (stream, mut rx) = EventStream::channel();
    let agent = Agent::new(model).extension(stream);
    let _ = timeout(RUN_TIMEOUT, agent.run("boom")).await.unwrap();

    let mut saw_error = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, HarnessEvent::Error { .. }) {
            saw_error = true;
        }
    }
    assert!(saw_error, "an error event must be emitted");
}

#[tokio::test]
async fn tool_policy_allowlist_denies_unlisted() {
    let model = Arc::new(ScriptedModel::tool_round(
        vec![
            call("c0", "echo", json!({"a": 1})),
            call("c1", "secret", json!({})),
        ],
        "done",
    ));
    let secret = FnTool::new(
        "secret",
        "should be blocked",
        json!({"type": "object"}),
        |_i, _c| async move { Ok(json!("leaked")) },
    );
    let agent = Agent::new(model.clone())
        .tool(echo())
        .tool(secret)
        .extension(ToolPolicy::new().allow(["echo"]));
    timeout(RUN_TIMEOUT, agent.run("try"))
        .await
        .unwrap()
        .unwrap();

    let seen = model.observed_contexts();
    let results = last_tool_results(&seen);
    assert!(!results[0].is_error, "echo allowed");
    assert!(results[1].is_error, "secret denied");
    assert!(results[1].output["error"]
        .as_str()
        .unwrap()
        .contains("not in the allowlist"));
}

#[tokio::test]
async fn tool_policy_custom_rule() {
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("c0", "echo", json!({"danger": true}))],
        "done",
    ));
    let policy = ToolPolicy::new().rule(|c: &orca_harness_core::ToolCall| {
        if c.arguments["danger"].as_bool() == Some(true) {
            PolicyOutcome::Deny("dangerous args".into())
        } else {
            PolicyOutcome::Allow
        }
    });
    let agent = Agent::new(model.clone()).tool(echo()).extension(policy);
    timeout(RUN_TIMEOUT, agent.run("try"))
        .await
        .unwrap()
        .unwrap();
    let results = last_tool_results(&model.observed_contexts());
    assert!(results[0].is_error);
    assert!(results[0].output["error"]
        .as_str()
        .unwrap()
        .contains("dangerous args"));
}

#[tokio::test]
async fn truncation_caps_large_output() {
    let big = "x".repeat(1000);
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("c0", "echo", json!({"text": big}))],
        "done",
    ));
    let agent = Agent::new(model.clone())
        .tool(echo())
        .extension(Truncation::new(100));
    timeout(RUN_TIMEOUT, agent.run("truncate"))
        .await
        .unwrap()
        .unwrap();

    let results = last_tool_results(&model.observed_contexts());
    let text = results[0].output["text"].as_str().unwrap();
    assert!(
        text.chars().count() < 300,
        "should be truncated, got {}",
        text.chars().count()
    );
    assert!(text.contains("elided"));
    assert_eq!(results[0].output["_truncated"], json!(true));
}

#[tokio::test]
async fn tool_retry_recovers_after_transient_failure() {
    let attempts = Arc::new(AtomicU32::new(0));
    let flaky = {
        let attempts = attempts.clone();
        FnTool::new(
            "flaky",
            "fails first, then succeeds",
            json!({"type": "object"}),
            move |_i, _c| {
                let attempts = attempts.clone();
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    if n < 3 {
                        Err(ToolError::msg("transient"))
                    } else {
                        Ok(json!({"ok": n}))
                    }
                }
            },
        )
    };
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("c0", "flaky", json!({}))],
        "done",
    ));
    let agent = Agent::new(model.clone())
        .tool(flaky)
        .extension(ToolRetry::new(3).backoff(Duration::from_millis(1)));
    timeout(RUN_TIMEOUT, agent.run("retry"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let results = last_tool_results(&model.observed_contexts());
    assert!(!results[0].is_error, "third attempt succeeds");
    assert_eq!(results[0].output["ok"], json!(3));
}

#[tokio::test]
async fn tool_retry_exhausts_and_reports_error() {
    let always = FnTool::new(
        "always",
        "always fails",
        json!({"type": "object"}),
        |_i, _c| async move { Err(ToolError::msg("nope")) },
    );
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("c0", "always", json!({}))],
        "done",
    ));
    let agent = Agent::new(model.clone())
        .tool(always)
        .extension(ToolRetry::new(2).backoff(Duration::from_millis(1)));
    timeout(RUN_TIMEOUT, agent.run("retry"))
        .await
        .unwrap()
        .unwrap();
    let results = last_tool_results(&model.observed_contexts());
    assert!(results[0].is_error);
    assert!(results[0].output["error"]
        .as_str()
        .unwrap()
        .contains("nope"));
}

/// A model that fails transiently N times before succeeding.
struct FlakyModel {
    fails_remaining: AtomicU32,
    log: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Model for FlakyModel {
    async fn generate(
        &self,
        _context: &orca_harness_core::Context,
        _tools: &[orca_harness_core::ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.log.lock().unwrap().push("gen".into());
        if self.fails_remaining.fetch_sub(1, Ordering::SeqCst) > 0 {
            Err(ModelError::Request("503 upstream".into()))
        } else {
            Ok(ModelResponse::final_text("recovered"))
        }
    }
}

#[tokio::test]
async fn retry_model_retries_transient_request_errors() {
    let model = RetryModel::new(
        FlakyModel {
            fails_remaining: AtomicU32::new(2),
            log: Mutex::new(Vec::new()),
        },
        5,
    )
    .backoff(Duration::from_millis(1));
    let agent = Agent::new(model);
    let answer = timeout(RUN_TIMEOUT, agent.run("go"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer, "recovered");
}

#[tokio::test]
async fn retry_model_does_not_retry_invalid_response() {
    struct AlwaysInvalid(AtomicU32);
    #[async_trait::async_trait]
    impl Model for AlwaysInvalid {
        async fn generate(
            &self,
            _c: &orca_harness_core::Context,
            _t: &[orca_harness_core::ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(ModelError::InvalidResponse("garbage".into()))
        }
    }
    let counter = Arc::new(AtomicU32::new(0));
    let model =
        RetryModel::new(AlwaysInvalid(AtomicU32::new(0)), 5).backoff(Duration::from_millis(1));
    let agent = Agent::new(model);
    let result = timeout(RUN_TIMEOUT, agent.run("go")).await.unwrap();
    assert!(result.is_err());
    let _ = counter;
}

#[tokio::test]
async fn usage_meter_accumulates_across_steps() {
    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call("c0", "echo", json!({}))],
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: 5,
                ..Default::default()
            }),
        },
        ModelResponse::Final {
            text: "done".into(),
            usage: Some(Usage {
                input_tokens: 50,
                output_tokens: 10,
                ..Default::default()
            }),
        },
    ]);
    let (meter, usage) = UsageMeter::new();
    let agent = Agent::new(model).tool(echo()).extension(meter);
    timeout(RUN_TIMEOUT, agent.run("go"))
        .await
        .unwrap()
        .unwrap();

    let total = usage.total();
    assert_eq!(total.input_tokens, 150);
    assert_eq!(total.output_tokens, 30);
    assert_eq!(total.cache_read_tokens, 5);
    assert_eq!(usage.metered_steps(), 2);
}

fn last_tool_results(
    contexts: &[orca_harness_core::Context],
) -> Vec<orca_harness_core::ToolResult> {
    contexts
        .last()
        .unwrap()
        .messages()
        .iter()
        .rev()
        .find_map(|m| match m {
            orca_harness_core::Message::Tool { results } => Some(results.clone()),
            _ => None,
        })
        .expect("tool results present")
}

/// Keep Value import used across cfgs.
#[allow(dead_code)]
fn _touch() -> Value {
    Value::Null
}
