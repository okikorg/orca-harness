//! The critical extensions, exercised end-to-end through the Agent with a
//! scripted fake LLM.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    Agent, CancellationToken, FnTool, Limits, Model, ModelError, ModelResponse, ToolError, Usage,
};
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
    // Expect: AgentStart, Assistant(thinking), Usage, ToolCall, ToolFinished,
    // ToolResult, Assistant(all done), Usage, Result.
    let tags: Vec<&str> = events
        .iter()
        .map(|e| match e {
            HarnessEvent::AgentStart => "start",
            HarnessEvent::AssistantDelta { .. } => "assistant_delta",
            HarnessEvent::ReasoningDelta { .. } => "reasoning_delta",
            HarnessEvent::ToolInputDelta { .. } => "tool_input_delta",
            HarnessEvent::Assistant { .. } => "assistant",
            HarnessEvent::ToolCall { .. } => "tool_call",
            HarnessEvent::ToolFinished { .. } => "tool_finished",
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
            "tool_finished",
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
    match &events[5] {
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

#[tokio::test(flavor = "multi_thread")]
async fn event_stream_emits_live_completion_before_slow_sibling_finishes() {
    let sleeper = FnTool::new(
        "sleeper",
        "sleeps for input milliseconds",
        json!({"type": "object"}),
        |input, _ctx| async move {
            let ms = input["ms"].as_u64().unwrap();
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(json!({"slept": ms}))
        },
    );
    let model = ScriptedModel::tool_round(
        vec![
            call("fast", "sleeper", json!({"ms": 10})),
            call("slow", "sleeper", json!({"ms": 500})),
        ],
        "done",
    );
    let (stream, mut rx) = EventStream::channel();
    let run = tokio::spawn(async move {
        Agent::new(model)
            .tool(sleeper)
            .extension(stream)
            .run("go")
            .await
    });

    timeout(Duration::from_millis(200), async {
        loop {
            if matches!(
                rx.recv().await,
                Some(HarnessEvent::ToolFinished { ref tool_call_id, .. }) if tool_call_id == "fast"
            ) {
                break;
            }
        }
    })
    .await
    .expect("fast completion must not wait for the slow sibling");
    assert!(!run.is_finished(), "slow sibling should still be executing");

    run.await.unwrap().unwrap();
    let mut result_ids = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let HarnessEvent::ToolResult { tool_call_id, .. } = event {
            result_ids.push(tool_call_id);
        }
    }
    assert_eq!(
        result_ids,
        ["fast", "slow"],
        "final results remain deterministic and call-ordered"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn event_stream_emits_completion_for_calls_cancelled_before_execution() {
    let slow = FnTool::new(
        "slow",
        "waits forever",
        json!({"type": "object"}),
        |_input, _ctx| async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(Value::Null)
        },
    );
    let model = ScriptedModel::tool_round(
        vec![
            call("waiting", "slow", json!({})),
            call("running", "slow", json!({})),
        ],
        "unreachable",
    );
    let (stream, mut rx) = EventStream::channel();
    let cancellation = CancellationToken::new();
    let run_token = cancellation.clone();
    let run = tokio::spawn(async move {
        Agent::new(model)
            .tool(slow)
            .extension(stream)
            .limits(Limits {
                max_parallel_tools: 1,
                ..Limits::default()
            })
            .run_with_cancellation("go", run_token)
            .await
    });

    tokio::time::sleep(Duration::from_millis(20)).await;
    cancellation.cancel();
    assert!(run.await.unwrap().is_err());

    let mut finished = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let HarnessEvent::ToolFinished {
            tool_call_id,
            is_error,
            ..
        } = event
        {
            finished.push((tool_call_id, is_error));
        }
    }
    finished.sort();
    assert_eq!(
        finished,
        [("running".into(), true), ("waiting".into(), true)]
    );
}

#[tokio::test]
async fn event_stream_serializes_to_platform_ndjson_tags() {
    // The tag names must match the platform NDJSON union.
    let ev = HarnessEvent::ToolFinished {
        tool_call_id: "c0".into(),
        tool_name: "echo".into(),
        is_error: false,
    };
    let s = serde_json::to_value(&ev).unwrap();
    assert_eq!(s["type"], json!("tool_finished"));
    assert_eq!(s["tool_call_id"], json!("c0"));

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

#[tokio::test]
async fn tool_retry_retries_data_failures_and_reports_last_output() {
    // A tool whose failures are *data*: it reports `success: false`
    // instead of returning Err. This is what shell/web_fetch do for
    // nonzero exits and 5xx responses — the classic case retry must
    // cover to be useful.
    let attempts = Arc::new(AtomicU32::new(0));
    let flaky = {
        let attempts = attempts.clone();
        FnTool::new(
            "probe",
            "reports success: false until the third try",
            json!({"type": "object"}),
            move |_i, _c| {
                let attempts = attempts.clone();
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    Ok(json!({"success": n >= 3, "attempt": n}))
                }
            },
        )
    };
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("c0", "probe", json!({}))],
        "done",
    ));

    // Three total attempts; every `success: false` output is retried.
    let agent = Agent::new(model.clone()).tool(flaky).extension(
        ToolRetry::new(3)
            .backoff(Duration::from_millis(1))
            .retry_ok_when(|call, out| {
                call.name == "probe" && out["success"].as_bool() == Some(false)
            }),
    );
    timeout(RUN_TIMEOUT, agent.run("retry"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let results = last_tool_results(&model.observed_contexts());
    assert!(
        !results[0].is_error,
        "rule-last output flows back as a result"
    );
    assert_eq!(results[0].output["attempt"], json!(3));

    // When every attempt is data-failed, the last real output (not a
    // synthetic error) is what the model sees.
    let always_fail = FnTool::new(
        "probe",
        "always success: false",
        json!({"type": "object"}),
        |_i, _c| async move { Ok(json!({"success": false, "status": 503})) },
    );
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("c0", "probe", json!({}))],
        "done",
    ));
    let agent = Agent::new(model.clone()).tool(always_fail).extension(
        ToolRetry::new(2)
            .backoff(Duration::from_millis(1))
            .retry_ok_when(|_, out| out["success"].as_bool() == Some(false)),
    );
    timeout(RUN_TIMEOUT, agent.run("retry"))
        .await
        .unwrap()
        .unwrap();
    let results = last_tool_results(&model.observed_contexts());
    assert!(!results[0].is_error, "last output is returned as-is");
    assert_eq!(results[0].output["status"], json!(503));
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

async fn assert_model_error_is_not_retried(error: ModelError) {
    struct AlwaysFails {
        calls: Arc<AtomicU32>,
        error: Mutex<Option<ModelError>>,
    }
    #[async_trait::async_trait]
    impl Model for AlwaysFails {
        async fn generate(
            &self,
            _c: &orca_harness_core::Context,
            _t: &[orca_harness_core::ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(self.error.lock().unwrap().take().unwrap())
        }
    }
    let counter = Arc::new(AtomicU32::new(0));
    let model = RetryModel::new(
        AlwaysFails {
            calls: Arc::clone(&counter),
            error: Mutex::new(Some(error)),
        },
        5,
    )
    .backoff(Duration::from_millis(1));
    let agent = Agent::new(model);
    let result = timeout(RUN_TIMEOUT, agent.run("go")).await.unwrap();
    assert!(result.is_err());
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_model_does_not_retry_invalid_response() {
    assert_model_error_is_not_retried(ModelError::InvalidResponse("garbage".into())).await;
}

#[tokio::test]
async fn retry_model_does_not_retry_authentication_errors() {
    assert_model_error_is_not_retried(ModelError::Authentication("login required".into())).await;
}

#[tokio::test]
async fn retry_model_preserves_streaming_and_retries_request_errors() {
    struct FlakyStreamingModel(Arc<AtomicU32>);

    #[async_trait::async_trait]
    impl Model for FlakyStreamingModel {
        async fn generate(
            &self,
            _context: &orca_harness_core::Context,
            _tools: &[orca_harness_core::ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            panic!("the streaming path must remain streaming");
        }

        async fn generate_streaming(
            &self,
            _context: &orca_harness_core::Context,
            _tools: &[orca_harness_core::ToolSchema],
            sink: &dyn orca_harness_core::DeltaSink,
        ) -> Result<ModelResponse, ModelError> {
            if self.0.fetch_add(1, Ordering::SeqCst) < 2 {
                return Err(ModelError::Request("connection dropped".into()));
            }
            sink.emit(orca_harness_core::ModelDelta::Text {
                text: "recovered".into(),
            })
            .await;
            Ok(ModelResponse::final_text("recovered"))
        }
    }

    let attempts = Arc::new(AtomicU32::new(0));
    let retries = Arc::new(Mutex::new(Vec::new()));
    let deltas = Arc::new(Mutex::new(Vec::new()));
    let sink_deltas = deltas.clone();
    let sink = move |delta| sink_deltas.lock().unwrap().push(delta);
    let retry_log = retries.clone();
    let model = RetryModel::new(FlakyStreamingModel(attempts.clone()), 10)
        .backoff(Duration::from_millis(1))
        .on_retry(move |attempt, max_attempts, _| {
            retry_log.lock().unwrap().push((attempt, max_attempts));
        });

    let response = model
        .generate_streaming(&orca_harness_core::Context::new(), &[], &sink)
        .await
        .unwrap();

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(*retries.lock().unwrap(), vec![(2, 10), (3, 10)]);
    assert!(matches!(response, ModelResponse::Final { ref text, .. } if text == "recovered"));
    assert_eq!(deltas.lock().unwrap().len(), 1);
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
