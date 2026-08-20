//! Wire-level tests: run a real request through a one-shot fake
//! chat-completions server, capture what went over the network, and check
//! what the adapter makes of the reply.

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use orca_harness_core::{Context, Model, ModelResponse, ToolSchema};
use orca_harness_model_openai::OpenAiModel;

/// Serve exactly one request: capture its body, reply with `response_json`.
async fn one_shot_server(response_json: String) -> (String, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let body = loop {
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0, "connection closed before full request arrived");
            raw.extend_from_slice(&buf[..n]);
            let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&raw[..split]).to_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .expect("request has content-length")
                .trim()
                .parse()
                .unwrap();
            if raw.len() >= split + 4 + length {
                break String::from_utf8(raw[split + 4..split + 4 + length].to_vec()).unwrap();
            }
        };
        let _ = tx.send(body);
        let reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            response_json.len(),
            response_json
        );
        stream.write_all(reply.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    });

    (format!("http://{addr}/v1"), rx)
}

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
    let (base_url, captured) = one_shot_server(completion.to_string()).await;

    let model = OpenAiModel::new("test-model")
        .base_url(base_url)
        .parallel_tool_calls(true);
    let mut context = Context::new();
    context.push_user("fan out");

    let response = model.generate(&context, &schemas()).await.unwrap();

    let sent: Value = serde_json::from_str(&captured.await.unwrap()).unwrap();
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
    let (base_url, captured) = one_shot_server(completion.to_string()).await;

    let model = OpenAiModel::new("test-model").base_url(base_url);
    let mut context = Context::new();
    context.push_user("hi");

    let response = model.generate(&context, &schemas()).await.unwrap();

    let sent: Value = serde_json::from_str(&captured.await.unwrap()).unwrap();
    assert!(sent.get("parallel_tool_calls").is_none());
    assert!(matches!(response, ModelResponse::Final { .. }));
}
