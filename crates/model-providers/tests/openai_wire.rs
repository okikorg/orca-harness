//! Wire-level tests: run a real request through a one-shot fake
//! chat-completions server, capture what went over the network, and check
//! what the adapter makes of the reply.

use serde_json::json;

use orca_harness_core::{Context, Model, ModelError, ModelResponse, ToolSchema};
use orca_harness_model_providers::openai::OpenAiModel;

#[path = "../src/test_server.rs"]
mod test_server;
use test_server::{json, serve, status};

fn schemas() -> Vec<ToolSchema> {
    vec![
        ToolSchema {
            name: "grep".into(),
            description: "search".into(),
            parameters: json!({"type": "object"}),
        },
        ToolSchema {
            name: "read_file".into(),
            description: "read".into(),
            parameters: json!({"type": "object"}),
        },
    ]
}

#[tokio::test]
async fn parallel_tool_calls_goes_over_the_wire_and_multi_call_batches_parse() {
    let completion = json!({
        "choices": [{"message": {"content": null, "tool_calls": [
            {"id": "a", "type": "function",
             "function": {"name": "grep", "arguments": "{\"pattern\":\"x\"}"}},
            {"id": "b", "type": "function",
             "function": {"name": "read_file", "arguments": "{\"path\":\"y\"}"}}
        ]}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2}
    });
    let (url, server) = serve(vec![json(completion.to_string())]).await;

    let model = OpenAiModel::new("test-model")
        .base_url(format!("{url}/v1"))
        .user_agent("orcacode/1.2.3")
        .parallel_tool_calls(true);
    let mut context = Context::new();
    context.push_user("fan out");

    let response = model.generate(&context, &schemas()).await.unwrap();

    let captured = server.await.unwrap().remove(0);
    assert!(captured.lower().contains("user-agent: orcacode/1.2.3"));
    let sent = captured.json();
    assert_eq!(sent["parallel_tool_calls"], json!(true));
    assert_eq!(sent["tools"].as_array().unwrap().len(), 2);

    let ModelResponse::ToolCalls { calls, .. } = response else {
        panic!("expected a tool-call batch");
    };
    assert_eq!(calls.len(), 2, "both calls must land in one batch");
    assert_eq!(calls[0].name, "grep");
    assert_eq!(calls[1].name, "read_file");
}

#[tokio::test]
async fn unset_knob_sends_no_parallel_tool_calls_field() {
    let completion = json!({
        "choices": [{"message": {"content": "done", "tool_calls": []}}]
    });
    let (url, server) = serve(vec![json(completion.to_string())]).await;

    let model = OpenAiModel::new("test-model").base_url(format!("{url}/v1"));
    let mut context = Context::new();
    context.push_user("hi");

    let response = model.generate(&context, &schemas()).await.unwrap();

    let sent = server.await.unwrap()[0].json();
    assert!(sent.get("parallel_tool_calls").is_none());
    assert!(matches!(response, ModelResponse::Final { .. }));
}

#[tokio::test]
async fn non_streaming_length_finish_reason_is_typed_and_retains_usage() {
    let completion = json!({
        "choices": [{
            "finish_reason": "length",
            "message": {"content": null, "tool_calls": [{
                "id": "a", "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\":\"unfinished"}
            }]}
        }],
        "usage": {"prompt_tokens": 7, "completion_tokens": 11}
    });
    let (url, _) = serve(vec![json(completion.to_string())]).await;
    let model = OpenAiModel::new("test-model").base_url(format!("{url}/v1"));

    let error = model
        .generate(&Context::new(), &schemas())
        .await
        .unwrap_err();
    match error {
        ModelError::OutputLimit { usage, .. } => {
            let usage = usage.expect("failed-turn usage retained");
            assert_eq!(usage.input_tokens, 7);
            assert_eq!(usage.output_tokens, 11);
        }
        other => panic!("expected OutputLimit, got {other:?}"),
    }
}

#[tokio::test]
async fn http_failures_preserve_retry_timing_and_permanent_classification() {
    use orca_harness_model_providers::http_error::retry_delay;
    for streaming in [false, true] {
        for (code, body, delay) in [
            (
                "429 Too Many Requests",
                r#"{"error":{"code":"rate_limit_exceeded"}}"#,
                Some(std::time::Duration::from_secs(7)),
            ),
            (
                "429 Too Many Requests",
                r#"{"error":{"code":"insufficient_quota"}}"#,
                None,
            ),
            ("400 Bad Request", "invalid", None),
            ("401 Unauthorized", "bad key", None),
        ] {
            let (url, server) = serve(vec![status(code, body).header("Retry-After", "7")]).await;
            let model = OpenAiModel::new("test").base_url(format!("{url}/v1"));
            let context = Context::new();
            let result = if streaming {
                model.generate_streaming(&context, &[], &|_| {}).await
            } else {
                model.generate(&context, &[]).await
            };
            assert_eq!(retry_delay(&result.unwrap_err()), delay);
            server.await.unwrap();
        }
    }
}
