//! Wire-level tests against a one-shot fake server: catalog parsing, and
//! generation delegating through the OpenAI adapter with OpenRouter's
//! attribution headers attached.

use serde_json::json;

use orca_harness_core::{Context, Model, ModelResponse};
use orca_harness_model_providers::openrouter::{list_models, OpenRouterModel};

#[path = "../src/test_server.rs"]
mod test_server;
use test_server::{json, serve};

#[tokio::test]
async fn list_models_parses_the_catalog_in_provider_order_and_sends_the_key() {
    let catalog = json!({"data": [
        {"id": "z/last", "context_length": 32768,
         "pricing": {"prompt": "0.000001", "completion": "0.000002"}},
        {"id": "a/first", "context_length": 128000,
         "pricing": {"prompt": "0", "completion": "0"}}
    ]});
    let (url, server) = serve(vec![json(catalog.to_string())]).await;

    let models = list_models(&format!("{url}/v1"), Some("sk-or-test"))
        .await
        .unwrap();

    let head = server.await.unwrap()[0].lower();
    assert!(head.starts_with("get /v1/models"));
    assert!(head.contains("bearer sk-or-test"));

    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "z/last");
    assert_eq!(models[1].summary(), "a/first  128k ctx  free");
    assert_eq!(
        models[0].summary(),
        "z/last  32k ctx  $1.00/M in $2.00/M out"
    );
}

#[tokio::test]
async fn generation_delegates_with_attribution_headers() {
    let completion = json!({
        "choices": [{"message": {"content": "hello", "tool_calls": []}}]
    });
    let (url, server) = serve(vec![json(completion.to_string())]).await;

    let model = OpenRouterModel::new("openai/gpt-4o")
        .base_url(format!("{url}/v1"))
        .api_key("sk-or-test")
        .user_agent("orcacode/1.2.3")
        .referer("https://example.com")
        .title("Orca Code")
        .categories("cli-agent")
        .reasoning_effort("high")
        .parallel_tool_calls(true);
    let mut context = Context::new();
    context.push_user("hi");

    let response = model.generate(&context, &[]).await.unwrap();

    let sent = server.await.unwrap().remove(0);
    let head = sent.lower();
    assert!(head.contains("bearer sk-or-test"));
    assert!(head.contains("user-agent: orcacode/1.2.3"));
    assert!(head.contains("http-referer: https://example.com"));
    assert!(head.contains("x-openrouter-title: orca code"));
    assert!(head.contains("x-openrouter-categories: cli-agent"));
    let body = sent.json();
    assert_eq!(body["model"], json!("openai/gpt-4o"));
    assert_eq!(body["reasoning"], json!({"effort": "high"}));
    assert!(body.get("reasoning_effort").is_none());

    let ModelResponse::Final { text, .. } = response else {
        panic!("expected a final answer");
    };
    assert_eq!(text, "hello");
}
