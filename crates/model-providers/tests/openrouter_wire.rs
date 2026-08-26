//! Wire-level tests against a one-shot fake server: catalog parsing, and
//! generation delegating through the OpenAI adapter with OpenRouter's
//! attribution headers attached.

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use orca_harness_core::{Context, Model, ModelResponse};
use orca_harness_model_providers::openrouter::{list_models, OpenRouterModel};

/// Captured request: lowercased head (request line + headers) and body.
struct Captured {
    head: String,
    body: String,
}

/// Serve exactly one request: capture it, reply with `response_json`.
async fn one_shot_server(response_json: String) -> (String, oneshot::Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let captured = loop {
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0, "connection closed before full request arrived");
            raw.extend_from_slice(&buf[..n]);
            let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&raw[..split]).to_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map(|v| v.trim().parse().unwrap())
                .unwrap_or(0);
            if raw.len() >= split + 4 + length {
                let body = String::from_utf8(raw[split + 4..split + 4 + length].to_vec()).unwrap();
                break Captured { head, body };
            }
        };
        let _ = tx.send(captured);
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

#[tokio::test]
async fn list_models_parses_the_catalog_sorted_and_sends_the_key() {
    let catalog = json!({"data": [
        {"id": "z/last", "context_length": 32768,
         "pricing": {"prompt": "0.000001", "completion": "0.000002"}},
        {"id": "a/first", "context_length": 128000,
         "pricing": {"prompt": "0", "completion": "0"}}
    ]});
    let (base_url, captured) = one_shot_server(catalog.to_string()).await;

    let models = list_models(&base_url, Some("sk-or-test")).await.unwrap();

    let sent = captured.await.unwrap();
    assert!(sent.head.starts_with("get /v1/models"));
    assert!(sent.head.contains("bearer sk-or-test"));

    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "a/first");
    assert_eq!(models[0].summary(), "a/first  128k ctx  free");
    assert_eq!(
        models[1].summary(),
        "z/last  32k ctx  $1.00/M in $2.00/M out"
    );
}

#[tokio::test]
async fn generation_delegates_with_attribution_headers() {
    let completion = json!({
        "choices": [{"message": {"content": "hello", "tool_calls": []}}]
    });
    let (base_url, captured) = one_shot_server(completion.to_string()).await;

    let model = OpenRouterModel::new("openai/gpt-4o")
        .base_url(base_url)
        .api_key("sk-or-test")
        .user_agent("orcacode/1.2.3")
        .referer("https://example.com")
        .title("Orca Code")
        .categories("cli-agent")
        .parallel_tool_calls(true);
    let mut context = Context::new();
    context.push_user("hi");

    let response = model.generate(&context, &[]).await.unwrap();

    let sent = captured.await.unwrap();
    assert!(sent.head.contains("bearer sk-or-test"));
    assert!(sent.head.contains("user-agent: orcacode/1.2.3"));
    assert!(sent.head.contains("http-referer: https://example.com"));
    assert!(sent.head.contains("x-openrouter-title: orca code"));
    assert!(sent.head.contains("x-openrouter-categories: cli-agent"));
    let body: Value = serde_json::from_str(&sent.body).unwrap();
    assert_eq!(body["model"], json!("openai/gpt-4o"));

    let ModelResponse::Final { text, .. } = response else {
        panic!("expected a final answer");
    };
    assert_eq!(text, "hello");
}
