//! Wire-level client identity coverage for the subscription-backed Codex adapter.

use std::sync::Arc;

use async_trait::async_trait;

use orca_harness_core::{Context, Model};
use orca_harness_model_providers::openai_codex::{
    BearerCredential, CodexCredential, CodexCredentialSource, CredentialError, CredentialSource,
    OpenAiCodexModel,
};

#[path = "../src/test_server.rs"]
mod test_server;
use test_server::{serve, sse, status};

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

#[tokio::test]
async fn generation_identifies_orcacode_without_impersonating_codex_cli() {
    let (url, server) = serve(vec![status("400 Bad Request", "")]).await;
    let model =
        OpenAiCodexModel::new("gpt-test", Arc::new(Credentials)).base_url(format!("{url}/codex"));
    let mut context = Context::new();
    context.push_user("hello");

    let error = model.generate(&context, &[]).await.unwrap_err();
    assert!(error.to_string().contains("400"));

    let head = server.await.unwrap()[0].lower();
    assert!(head.contains("user-agent: orcacode/"));
    assert!(head.contains("originator: orcacode"));
    assert!(!head.contains("codex_cli_rs"));
    assert!(head.contains("chatgpt-account-id: account-test"));
}

#[tokio::test]
async fn codex_throttle_preserves_retry_after() {
    for streaming in [false, true] {
        let throttled = status("429 Too Many Requests", "").header("Retry-After", "7");
        let (url, server) = serve(vec![throttled]).await;
        let model =
            OpenAiCodexModel::new("test", Arc::new(Credentials)).base_url(format!("{url}/codex"));
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
        server.await.unwrap();
    }
}

#[tokio::test]
async fn streamed_tool_batch_replays_reasoning_once_on_the_next_request() {
    use orca_harness_core::{ModelResponse, ToolResult};
    use serde_json::json;
    use std::time::Duration;

    tokio::time::timeout(Duration::from_secs(5), async {
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
        let replies = [&first[..], &second[..]].into_iter().map(|events| {
            sse(events.iter().map(|event| format!("data: {event}\r\n\r\n")).collect::<String>())
        }).collect();
        let (url, server) = serve(replies).await;
        let model = OpenAiCodexModel::new("test", Arc::new(Credentials))
            .base_url(format!("{url}/codex"))
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
        let captured = server.await.unwrap();
        let (initial, next) = (captured[0].json(), captured[1].json());
        assert_eq!(initial["prompt_cache_key"], "stable");
        assert_eq!(next["prompt_cache_key"], "stable");
        let input = next["input"].as_array().unwrap();
        assert_eq!(input.iter().filter(|item| item["type"] == "reasoning").count(), 1);
        assert_eq!(input[1], reasoning);
        assert_eq!(input[2]["call_id"], "c1");
        assert_eq!(input[3]["call_id"], "c2");
        assert_eq!(input[4]["type"], "function_call_output");
    }).await.expect("Codex conversation must finish");
}
