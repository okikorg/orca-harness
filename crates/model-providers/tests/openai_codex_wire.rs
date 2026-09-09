//! Wire-level client identity coverage for the subscription-backed Codex adapter.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use orca_harness_core::{Context, Model};
use orca_harness_model_providers::openai_codex::{
    BearerCredential, CodexCredential, CodexCredentialSource, CredentialError, CredentialSource,
    OpenAiCodexModel,
};

#[derive(Clone)]
struct Credentials;

#[async_trait]
impl CredentialSource for Credentials {
    async fn credential(&self) -> Result<BearerCredential, CredentialError> {
        Ok(bearer())
    }

    async fn refresh(&self) -> Result<BearerCredential, CredentialError> {
        Ok(bearer())
    }
}

#[async_trait]
impl CodexCredentialSource for Credentials {
    async fn codex_credential(&self) -> Result<CodexCredential, CredentialError> {
        Ok(CodexCredential {
            bearer: bearer(),
            account_id: "account-test".into(),
        })
    }

    async fn refresh_codex(
        &self,
        _rejected_access_token: &str,
    ) -> Result<CodexCredential, CredentialError> {
        self.codex_credential().await
    }
}

fn bearer() -> BearerCredential {
    BearerCredential {
        access_token: "token-test".into(),
        expires_at: None,
    }
}

async fn rejecting_server(reply: &'static [u8]) -> (String, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let head = loop {
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0, "connection closed before request headers arrived");
            raw.extend_from_slice(&buf[..n]);
            if let Some(split) = raw.windows(4).position(|part| part == b"\r\n\r\n") {
                break String::from_utf8_lossy(&raw[..split]).to_lowercase();
            }
        };
        let _ = tx.send(head);
        stream.write_all(reply).await.unwrap();
        stream.shutdown().await.unwrap();
    });

    (format!("http://{addr}/codex"), rx)
}

#[tokio::test]
async fn generation_identifies_orcacode_without_impersonating_codex_cli() {
    let (base_url, captured) = rejecting_server(
        b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let model = OpenAiCodexModel::new("gpt-test", Arc::new(Credentials)).base_url(base_url);
    let mut context = Context::new();
    context.push_user("hello");

    let error = model.generate(&context, &[]).await.unwrap_err();
    assert!(error.to_string().contains("400"));

    let head = captured.await.unwrap();
    assert!(head.contains("user-agent: orcacode/"));
    assert!(head.contains("originator: orcacode"));
    assert!(!head.contains("codex_cli_rs"));
    assert!(head.contains("chatgpt-account-id: account-test"));
}

#[tokio::test]
async fn codex_throttle_preserves_retry_after() {
    for streaming in [false, true] {
        let (url, captured) = rejecting_server(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
        let model = OpenAiCodexModel::new("test", Arc::new(Credentials)).base_url(url);
        let context = Context::new();
        let result = if streaming {
            model.generate_streaming(&context, &[], &|_| {}).await
        } else {
            model.generate(&context, &[]).await
        };
        assert_eq!(
            orca_harness_model_providers::http_error::retry_delay(&result.unwrap_err()),
            Some(std::time::Duration::from_secs(7))
        );
        captured.await.unwrap();
    }
}

#[tokio::test]
async fn streamed_tool_batch_replays_reasoning_once_on_the_next_request() {
    use orca_harness_core::{ModelResponse, ToolResult};
    use serde_json::{json, Value};
    use std::time::Duration;

    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, mut captured) = tokio::sync::mpsc::unbounded_channel();
        let reasoning = json!({"type":"reasoning","id":"r1","summary":[],"encrypted_content":"opaque"});
        let first = [
            json!({"type":"response.output_item.done","item":reasoning}),
            json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"one","arguments":"{}"}}),
            json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"c2","name":"two","arguments":"{}"}}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":12,"output_tokens":3,"input_tokens_details":{"cached_tokens":4}}}}),
        ];
        let second = [
            json!({"type":"response.output_text.delta","delta":"finished"}),
            json!({"type":"response.completed","response":{}}),
        ];
        let replies: Vec<_> = [&first[..], &second[..]].into_iter().map(|events| {
            events.iter().map(|event| format!("data: {event}\r\n\r\n")).collect::<String>()
        }).collect();
        let server = tokio::spawn(async move {
            for reply in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut raw = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let size = stream.read(&mut chunk).await.unwrap();
                    assert!(size > 0);
                    raw.extend_from_slice(&chunk[..size]);
                    let Some(split) = raw.windows(4).position(|part| part == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&raw[..split]).to_lowercase();
                    let length: usize = head.lines().find_map(|line| line.strip_prefix("content-length:")).unwrap().trim().parse().unwrap();
                    if raw.len() < split + 4 + length { continue; }
                    tx.send(serde_json::from_slice::<Value>(&raw[split+4..split+4+length]).unwrap()).unwrap();
                    break;
                }
                let response = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len());
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        let model = OpenAiCodexModel::new("test", Arc::new(Credentials))
            .base_url(format!("http://{address}/codex"))
            .prompt_cache_key("stable");
        let mut context = Context::new();
        context.push_user("use tools");
        let response = model.generate(&context, &[]).await.unwrap();
        let ModelResponse::ToolCalls { calls, usage, .. } = response else { panic!("expected tool calls") };
        assert_eq!(calls.len(), 2);
        let usage = usage.unwrap();
        assert_eq!((usage.input_tokens, usage.cache_read_tokens, usage.output_tokens), (8, 4, 3));
        context.push_assistant_tool_calls(None, calls.clone());
        context.append_tool_results(calls.iter().map(|call| ToolResult::ok(call, json!("ok"))).collect());
        let response = model.generate_streaming(&context, &[], &|_| {}).await.unwrap();
        assert!(matches!(response, ModelResponse::Final { text, .. } if text == "finished"));
        let initial = captured.recv().await.unwrap();
        let next = captured.recv().await.unwrap();
        assert_eq!(initial["prompt_cache_key"], "stable");
        assert_eq!(next["prompt_cache_key"], "stable");
        let input = next["input"].as_array().unwrap();
        assert_eq!(input.iter().filter(|item| item["type"] == "reasoning").count(), 1);
        assert_eq!(input[1], reasoning);
        assert_eq!(input[2]["call_id"], "c1");
        assert_eq!(input[3]["call_id"], "c2");
        assert_eq!(input[4]["type"], "function_call_output");
        server.await.unwrap();
    }).await.expect("Codex conversation must finish");
}
