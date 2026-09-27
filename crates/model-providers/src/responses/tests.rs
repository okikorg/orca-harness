use super::*;
use crate::test_server::{serve, sse, Reply};
use orca_harness_core::{Image, ToolCall, ToolResult};
use serde_json::json;

/// One Responses SSE reply carrying `events`.
fn events(events: &[Value]) -> Reply {
    sse(events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>())
}

fn completed_text(text: &str) -> Reply {
    events(&[
        json!({"type":"response.output_text.delta","delta":text}),
        json!({"type":"response.completed","response":{}}),
    ])
}

#[tokio::test]
async fn api_key_request_and_sse_usage() {
    let (url, server) = serve(vec![events(&[
        json!({"type":"response.output_text.delta","delta":"hello"}),
        json!({"type":"response.completed","response":{"usage":{"input_tokens":9,"output_tokens":3,"input_tokens_details":{"cached_tokens":2}}}}),
    ])]).await;
    let model = ResponsesModel::new("gpt-test")
        .base_url(format!("{url}/openai/v1/?api-version=preview"))
        .api_key("secret")
        .header("api-key", "azure-secret")
        .max_tokens(42)
        .reasoning_effort("low");
    let mut context = Context::new();
    context.push_system("instructions");
    context.push_user_with_images(
        "look",
        vec![Image {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        }],
    );
    let response = model.generate(&context, &[]).await.unwrap();
    assert!(matches!(response, ModelResponse::Final { text, .. } if text == "hello"));
    let request = &server.await.unwrap()[0];
    assert!(request
        .head
        .starts_with("POST /openai/v1/responses?api-version=preview HTTP/1.1"));
    assert!(request.lower().contains("authorization: bearer secret"));
    assert!(request.lower().contains("api-key: azure-secret"));
    let body = request.json();
    assert_eq!(body["max_output_tokens"], 42);
    assert_eq!(body["reasoning"]["effort"], "low");
    assert_eq!(
        body["input"][0]["content"][1]["image_url"],
        "data:image/png;base64,AAAA"
    );
}

#[tokio::test]
async fn encrypted_reasoning_replayed_on_tool_continuation() {
    let call = ToolCall {
        id: "call-1".into(),
        name: "shell".into(),
        arguments: json!({"cmd":"pwd"}),
    };
    let (url, server) = serve(vec![
        events(&[
            json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"r1","summary":[],"encrypted_content":"secret-reasoning"}}),
            json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"call-1","name":"shell","arguments":"{\"cmd\":\"pwd\"}"}}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":2}}}),
        ]),
        completed_text("done"),
    ])
    .await;
    let model = ResponsesModel::new("gpt-test").base_url(&url);
    let mut context = Context::new();
    context.push_user("run");
    assert!(matches!(
        model.generate(&context, &[]).await.unwrap(),
        ModelResponse::ToolCalls { .. }
    ));
    context.push_assistant_tool_calls(None, vec![call.clone()]);
    context.append_tool_results(vec![ToolResult::ok(&call, json!({"stdout":"/tmp"}))]);
    model.generate(&context, &[]).await.unwrap();
    let body = server.await.unwrap()[1].json();
    let items = body["input"].as_array().unwrap();
    assert_eq!(
        items.iter().find(|v| v["type"] == "reasoning").unwrap()["encrypted_content"],
        "secret-reasoning"
    );
    assert_eq!(items.last().unwrap()["type"], "function_call_output");
}

#[tokio::test]
async fn full_context_replays_every_tool_turn_including_parallel_calls() {
    let (url, server) = serve(vec![completed_text("done")]).await;
    let model = ResponsesModel::new("gpt-test").base_url(url);
    let mut context = Context::new();
    context.push_user("start");
    for turn in 0..3 {
        let calls: Vec<_> = (0..2)
            .map(|n| ToolCall {
                id: format!("call-{turn}-{n}"),
                name: "shell".into(),
                arguments: json!({}),
            })
            .collect();
        let reasoning: Arc<[Value]> = vec![json!({"type":"reasoning", "id":format!("r{turn}"),
            "summary":[], "encrypted_content":format!("secret-{turn}")})]
        .into();
        for call in &calls {
            model
                .reasoning_by_call
                .lock()
                .await
                .insert(call.id.clone(), reasoning.clone());
        }
        context.push_assistant_tool_calls(None, calls.clone());
        context.append_tool_results(
            calls
                .iter()
                .map(|c| ToolResult::ok(c, json!({"ok":true})))
                .collect(),
        );
    }
    model.generate(&context, &[]).await.unwrap();
    let body = server.await.unwrap()[0].json();
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 16);
    for turn in 0..3 {
        let offset = 1 + turn * 5;
        assert_eq!(input[offset]["encrypted_content"], format!("secret-{turn}"));
        assert_eq!(input[offset + 1]["call_id"], format!("call-{turn}-0"));
        assert_eq!(input[offset + 2]["call_id"], format!("call-{turn}-1"));
        assert_eq!(input[offset + 3]["type"], "function_call_output");
    }
}

#[tokio::test]
async fn tool_sse_streams_input_and_truncated_stream_is_generic_error() {
    let (url, server) = serve(vec![
        events(&[
            json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"shell","arguments":"{}"}}),
            json!({"type":"response.completed","response":{}}),
        ]),
        events(&[json!({"type":"response.output_text.delta","delta":"partial"})]),
    ])
    .await;
    let model = ResponsesModel::new("gpt-4.1").base_url(url);
    let seen = std::sync::Mutex::new(Vec::new());
    let result = model
        .generate_streaming(&Context::new(), &[], &|delta| {
            seen.lock().unwrap().push(delta);
        })
        .await
        .unwrap();
    assert!(matches!(result, ModelResponse::ToolCalls { calls, .. } if calls[0].id == "c1"));
    assert!(
        matches!(seen.lock().unwrap().as_slice(), [orca_harness_core::ModelDelta::ToolInput { text }] if text == "{}")
    );

    let error = model.generate(&Context::new(), &[]).await.unwrap_err();
    assert!(error.to_string().contains("Responses stream ended"));
    assert!(!error.to_string().contains("Codex"));
    server.await.unwrap();
}

#[tokio::test]
async fn non_reasoning_and_non_openai_responses_omit_optional_reasoning_fields() {
    let (url, server) = serve(vec![
        completed_text("hello"),
        events(&[
            json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"r1","summary":[]}}),
            json!({"type":"response.output_text.delta","delta":"ok"}),
            json!({"type":"response.completed","response":{}}),
        ]),
    ])
    .await;
    ResponsesModel::new("grok-test")
        .base_url(&url)
        .encrypted_reasoning(false)
        .generate(&Context::new(), &[])
        .await
        .unwrap();
    let response = ResponsesModel::new("gpt-4.1")
        .base_url(url)
        .generate(&Context::new(), &[])
        .await
        .unwrap();
    assert!(matches!(response, ModelResponse::Final { text, .. } if text == "ok"));
    let captured = server.await.unwrap();
    let body = captured[0].json();
    assert!(body.get("reasoning").is_none());
    assert!(body.get("include").is_none());
    let body = captured[1].json();
    assert!(body.get("reasoning").is_none());
    assert_eq!(body["include"][0], "reasoning.encrypted_content");
}
