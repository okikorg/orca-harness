//! Registry factory contracts, exercised without external services or credentials.
use std::{collections::BTreeSet, time::Duration};

use orca_harness_core::{Context, Model, ModelError, ModelResponse};
use orca_harness_model_providers::{
    registry::{Protocol, ALL},
    ProviderModel, ProviderPreset,
};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};

const LIMIT: Duration = Duration::from_secs(5);
const KEY: &str = "registry-wire-key";
const PROVIDER_IDS: [&str; 44] = [
    "amazon-bedrock",
    "ant-ling",
    "anthropic",
    "azure-openai-responses",
    "baseten",
    "cerebras",
    "cloudflare-ai-gateway",
    "cloudflare-workers-ai",
    "cursor",
    "databricks-unity-gateway",
    "deepseek",
    "fireworks",
    "github-copilot",
    "google",
    "google-vertex",
    "groq",
    "huggingface",
    "kimi-coding",
    "meta",
    "minimax",
    "minimax-cn",
    "mistral",
    "moonshotai",
    "moonshotai-cn",
    "nvidia",
    "openai",
    "openai-codex",
    "opencode",
    "opencode-go",
    "openrouter",
    "qwen-token-plan",
    "qwen-token-plan-cn",
    "qwen-token-plan-individual",
    "radius",
    "snowflake-cortex",
    "together",
    "vercel-ai-gateway",
    "xai",
    "xiaomi",
    "xiaomi-token-plan-ams",
    "xiaomi-token-plan-cn",
    "xiaomi-token-plan-sgp",
    "zai",
    "zai-coding-cn",
];

#[test]
fn exact_provider_inventory_plus_three_legacy_presets() {
    let mut expected: BTreeSet<_> = PROVIDER_IDS.into_iter().collect();
    assert_eq!(expected.len(), 44);
    expected.extend(["vercel", "cheaperinference", "local"]);
    assert_eq!(ALL.len(), 47);
    assert_eq!(
        ALL.iter().map(|p| p.id()).collect::<BTreeSet<_>>(),
        expected
    );
    for &preset in ALL {
        assert_eq!(ProviderPreset::from_id(preset.id()), Some(preset));
        assert!(!preset.default_model().is_empty(), "{}", preset.id());
    }
    assert_eq!(ProviderPreset::from_id("not-a-provider"), None);
}

fn sse(events: Vec<Value>) -> String {
    events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

fn fixture(protocol: Protocol) -> (&'static str, String) {
    match protocol {
        Protocol::ChatCompletions => ("application/json", json!({
            "choices": [{"message": {"role": "assistant", "content": "hello"}, "finish_reason": "stop"}]
        }).to_string()),
        Protocol::Anthropic => ("text/event-stream", sse(vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
            json!({"type":"message_stop"}),
        ])),
        Protocol::Responses => ("text/event-stream", sse(vec![
            json!({"type":"response.output_text.delta","delta":"hello"}),
            json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}),
        ])),
        Protocol::Google | Protocol::Vertex => ("text/event-stream", sse(vec![
            json!({"candidates":[{"content":{"role":"model","parts":[{"text":"hello"}]},"finishReason":"STOP"}]}),
        ])),
        other => panic!("specialized protocol {other:?} needs its own wire suite"),
    }
}

// Join the server so parsing/writing failures are not hidden in detached tasks.
// Both the server's accept/read/write and the caller's generation are bounded.
async fn server(protocol: Protocol) -> (String, JoinHandle<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/wire/v1", listener.local_addr().unwrap());
    let (content_type, body) = fixture(protocol);
    let task = tokio::spawn(async move {
        timeout(LIMIT, async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0; 4096];
            let captured = loop {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0, "connection closed before request completed");
                raw.extend_from_slice(&buf[..n]);
                let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                let head = String::from_utf8(raw[..split].to_vec()).unwrap();
                let length: usize = head.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                }).expect("JSON request has content-length");
                if raw.len() >= split + 4 + length {
                    break (head, serde_json::from_slice(&raw[split + 4..split + 4 + length]).unwrap());
                }
            };
            let reply = format!("HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            stream.write_all(reply.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
            captured
        }).await.expect("mock server timed out")
    });
    (url, task)
}

#[tokio::test]
async fn every_default_standard_protocol_uses_the_expected_wire_contract() {
    let mut tested = 0;
    for &preset in ALL {
        let id = preset.id();
        if id == "github-copilot" {
            continue;
        }
        let model_id = preset.default_model();
        // OpenRouter normalizes even catalog routes advertising another dialect.
        let protocol = if id == "openrouter" {
            Protocol::ChatCompletions
        } else {
            preset.route(model_id).protocol
        };
        match protocol {
            Protocol::Bedrock | Protocol::Cursor | Protocol::PiMessages | Protocol::Codex => {
                continue
            }
            _ => {}
        }
        let (url, task) = server(protocol).await;
        let model = ProviderModel::new(preset, model_id)
            .base_url(url)
            .api_key(KEY);
        let mut context = Context::new();
        context.push_user("hi");
        let result = timeout(LIMIT, model.generate(&context, &[]))
            .await
            .unwrap_or_else(|_| panic!("{id}: generation timed out"))
            .unwrap_or_else(|error| panic!("{id}: {error}"));
        let (head, body) = task.await.unwrap();
        let path = match protocol {
            Protocol::ChatCompletions => "/wire/v1/chat/completions".to_owned(),
            Protocol::Anthropic => "/wire/v1/messages".to_owned(),
            Protocol::Responses => "/wire/v1/responses".to_owned(),
            Protocol::Google | Protocol::Vertex => {
                format!("/wire/v1/models/{model_id}:streamGenerateContent?alt=sse")
            }
            _ => unreachable!(),
        };
        assert_eq!(
            head.lines().next().unwrap(),
            format!("POST {path} HTTP/1.1"),
            "{id}"
        );
        if !matches!(protocol, Protocol::Google | Protocol::Vertex) {
            assert_eq!(body["model"], model_id, "{id}");
        }
        let auth = match protocol {
            Protocol::Anthropic if id == "cloudflare-ai-gateway" => {
                assert!(!head.to_lowercase().contains("\r\nx-api-key:"));
                assert!(!head.to_lowercase().contains("\r\nauthorization:"));
                format!("cf-aig-authorization: Bearer {KEY}")
            }
            Protocol::Anthropic
                if !matches!(id, "databricks-unity-gateway" | "snowflake-cortex") =>
            {
                format!("x-api-key: {KEY}")
            }
            Protocol::Google | Protocol::Vertex => format!("x-goog-api-key: {KEY}"),
            _ => format!("authorization: Bearer {KEY}"),
        };
        assert!(
            head.lines().any(|line| line.eq_ignore_ascii_case(&auth)),
            "{id}: missing {auth}"
        );
        if id == "azure-openai-responses" {
            assert!(
                head.lines().any(|line| line == format!("api-key: {KEY}")),
                "{id}"
            );
        }
        let ModelResponse::Final { text, .. } = result else {
            panic!("{id}: expected final response")
        };
        assert_eq!(text, "hello", "{id}");
        tested += 1;
    }
    assert_eq!(
        tested, 42,
        "only Bedrock, Cursor, Copilot, Pi and Codex are skipped"
    );
}

#[tokio::test]
async fn required_keys_reject_missing_empty_and_whitespace_credentials() {
    // A bound but unserved endpoint ensures a regression cannot hit a real service.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    for &preset in ALL {
        let Some(key_env) = preset.key_env() else {
            continue;
        };
        for key in [None, Some(""), Some(" \t ")] {
            let mut model = ProviderModel::new(preset, preset.default_model()).base_url(&url);
            if let Some(key) = key {
                model = model.api_key(key);
            }
            let result = timeout(LIMIT, model.generate(&Context::new(), &[]))
                .await
                .unwrap_or_else(|_| panic!("{}: missing key reached transport", preset.id()));
            match result {
                Err(ModelError::Authentication(message)) => {
                    assert!(message.contains(preset.id()), "{message}");
                    assert!(message.contains(key_env), "{message}");
                }
                _ => panic!("{}: expected authentication error for {key:?}", preset.id()),
            }
        }
    }
    assert!(
        timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err(),
        "missing credentials must not reach transport"
    );
}
