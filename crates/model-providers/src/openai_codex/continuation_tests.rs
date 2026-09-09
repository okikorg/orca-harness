use super::*;
use orca_harness_core::{ToolCall, ToolResult};
use serde_json::json;

pub(super) struct UnusedCredentials;
#[async_trait]
impl CredentialSource for UnusedCredentials {
    async fn credential(&self) -> Result<BearerCredential, CredentialError> {
        unreachable!()
    }
    async fn refresh(&self) -> Result<BearerCredential, CredentialError> {
        unreachable!()
    }
}
#[async_trait]
impl CodexCredentialSource for UnusedCredentials {
    async fn codex_credential(&self) -> Result<CodexCredential, CredentialError> {
        unreachable!()
    }
    async fn refresh_codex(&self, _: &str) -> Result<CodexCredential, CredentialError> {
        unreachable!()
    }
}

#[tokio::test]
async fn tool_batch_shares_reasoning_and_consumed_calls_are_removed() {
    let model = OpenAiCodexModel::new("test", Arc::new(UnusedCredentials));
    let calls: Vec<_> = (0..16)
        .map(|i| ToolCall {
            id: format!("call_{i}"),
            name: "tool".into(),
            arguments: json!({}),
        })
        .collect();
    let reasoning: Arc<[serde_json::Value]> = vec![json!({
        "type":"reasoning", "id":"r", "summary":[], "encrypted_content":"opaque".repeat(1024)
    })]
    .into();
    let collected = Collected {
        response: ModelResponse::ToolCalls {
            content: None,
            calls: calls.clone(),
            usage: None,
        },
        reasoning: reasoning.clone(),
    };
    model.remember_reasoning(&Context::new(), &collected).await;
    {
        let pending = model.reasoning_by_call.lock().await;
        assert_eq!(pending.len(), calls.len());
        assert!(pending.values().all(|value| Arc::ptr_eq(value, &reasoning)));
    }
    let mut context = Context::new();
    context.push_assistant_tool_calls(None, calls.clone());
    context.append_tool_results(
        calls
            .iter()
            .map(|call| ToolResult::ok(call, json!("ok")))
            .collect(),
    );
    let continuation = model.continuation_for(&context).await;
    assert!(Arc::ptr_eq(&continuation, &reasoning));
    let body = request::body("test", &context, &[], true, &continuation, None, None);
    assert_eq!(body["input"][0], reasoning[0]);
    assert_eq!(body["input"].as_array().unwrap().len(), 33);
    model
        .remember_reasoning(
            &context,
            &Collected {
                response: ModelResponse::final_text("done"),
                reasoning: Arc::default(),
            },
        )
        .await;
    assert!(model.reasoning_by_call.lock().await.is_empty());
    assert!(model.continuation_for(&context).await.is_empty());
    // A selected continuation remains valid after its cache entries are removed.
    assert_eq!(continuation[0], reasoning[0]);
}
