//! Live registry discovery against local credential-sensitive servers.
use orca_harness_core::{Context, Model, ModelError};
use orca_harness_model_providers::{ModelInfo, Protocol, ProviderModel, ProviderPreset};

#[path = "../src/test_server.rs"]
mod test_server;
use test_server::{json, serve, status, Captured};
use tokio::task::JoinHandle;

/// Serve `(status, body)` JSON replies in order.
async fn server(
    responses: Vec<(&'static str, &'static str)>,
) -> (String, JoinHandle<Vec<Captured>>) {
    serve(responses.into_iter().map(|(s, b)| status(s, b)).collect()).await
}

#[allow(clippy::result_large_err)] // mirrors `ProviderModel::models`
async fn discover(
    preset: ProviderPreset,
    url: String,
    key: &str,
) -> Result<Vec<ModelInfo>, ModelError> {
    ProviderModel::new(preset, "manual")
        .base_url(url)
        .api_key(key)
        .models()
        .await
}

#[tokio::test]
async fn credentials_change_live_results_without_synthetic_metadata() {
    let (url, task) = server(vec![
        (
            "200 OK",
            r#"{"data":[{"id":"account-a","unknown":true,"url":"https://untrusted.invalid"}]}"#,
        ),
        (
            "200 OK",
            r#"{"data":[{"id":"account-b","name":"Remote name","context_length":123}]}"#,
        ),
        ("200 OK", r#"{"data":[]}"#),
    ])
    .await;
    let model = ProviderModel::new(ProviderPreset::OpenAi, "manual")
        .base_url(url)
        .api_key("first");
    let models = model.models().await.unwrap();
    assert_eq!(models[0].id, "account-a");
    assert!(models[0].name.is_none());
    assert!(models[0].context_length.is_none());
    assert!(models[0].pricing.is_none());
    assert!(models[0].reasoning.is_none());
    let model = model.api_key("second");
    let models = model.models().await.unwrap();
    assert_eq!(models[0].id, "account-b");
    assert_eq!(models[0].context_length, Some(123));
    assert!(model.models().await.unwrap().is_empty());
    let requests = task.await.unwrap();
    assert!(requests[0].head.contains("authorization: Bearer first"));
    assert!(requests[1].head.contains("authorization: Bearer second"));
    assert!(requests.iter().all(|r| r.head.starts_with("GET /models ")));
}

#[tokio::test]
async fn auth_and_invalid_responses_are_not_catalogs() {
    for (status, body) in [
        ("401 Unauthorized", "{}"),
        ("403 Forbidden", "{}"),
        ("200 OK", "{}"),
        ("200 OK", r#"{"data":[{}]}"#),
    ] {
        let (url, task) = server(vec![(status, body)]).await;
        let result = discover(ProviderPreset::OpenAi, url, "bad").await;
        assert!(result.is_err());
        if status.starts_with("401") || status.starts_with("403") {
            assert!(matches!(result, Err(ModelError::Authentication(_))));
        }
        task.await.unwrap();
    }
    assert!(matches!(
        ProviderModel::new(ProviderPreset::OpenAi, "manual")
            .models()
            .await,
        Err(ModelError::Authentication(_))
    ));
}

#[tokio::test]
async fn native_anthropic_and_google_metadata() {
    for (preset, body, header) in [
        (
            ProviderPreset::Anthropic,
            r#"{"data":[{"id":"remote","display_name":"Remote","max_input_tokens":2048}],"has_more":false}"#,
            "x-api-key: secret",
        ),
        (
            ProviderPreset::Google,
            r#"{"models":[{"name":"models/remote","displayName":"Remote","inputTokenLimit":2048,"supportedGenerationMethods":["generateContent"]}]}"#,
            "x-goog-api-key: secret",
        ),
    ] {
        let (url, task) = server(vec![("200 OK", body)]).await;
        let models = discover(preset, url, "secret").await.unwrap();
        assert_eq!(models[0].id, "remote");
        assert_eq!(models[0].context_length, Some(2048));
        assert!(task.await.unwrap()[0].head.contains(header));
    }
}

#[tokio::test]
async fn unresolved_routing_and_unsupported_discovery_are_actionable() {
    let model = ProviderModel::new(ProviderPreset::Opencode, "anything").api_key("key");
    let error = model
        .generate(&Context::default(), &[])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("protocol"));
    let error = model.models().await.unwrap_err().to_string();
    assert!(error.contains("discovery unavailable"));
}

#[tokio::test]
async fn manual_model_generates_without_discovery_even_on_mixed_provider() {
    let (url, task) = server(vec![(
        "200 OK",
        r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
    )])
    .await;
    ProviderModel::new(ProviderPreset::Opencode, "never-listed")
        .protocol(Protocol::ChatCompletions)
        .base_url(url)
        .api_key("key")
        .generate(&Context::default(), &[])
        .await
        .unwrap();
    let requests = task.await.unwrap();
    assert!(requests[0].head.starts_with("POST /chat/completions "));
}

#[tokio::test]
async fn discovery_does_not_follow_redirects() {
    let redirect = status("302 Found", "").header("Location", "/models");
    let (url, task) = serve(vec![redirect]).await;
    assert!(discover(ProviderPreset::OpenAi, url, "secret")
        .await
        .is_err());
    assert_eq!(task.await.unwrap().len(), 1);
}

#[tokio::test]
async fn openrouter_expands_gateway_efforts_and_vercel_remote_fields() {
    let (url, task) = server(vec![(
        "200 OK",
        r#"{"data":[{"id":"remote","reasoning":{"supported_efforts":null}}]}"#,
    )])
    .await;
    let models = discover(ProviderPreset::OpenRouter, url, "key")
        .await
        .unwrap();
    assert_eq!(
        models[0].reasoning.as_ref().unwrap().supported_efforts,
        Some(orca_harness_model_providers::SupportedEfforts::Listed(
            ["max", "xhigh", "high", "medium", "low", "minimal", "none"]
                .map(String::from)
                .to_vec()
        ))
    );
    task.await.unwrap();
    let (url, task) = server(vec![("200 OK", r#"{"data":[{"id":"remote","context_window":456,"reasoning_options":[{"type":"effort","values":["remote-effort"]}]}]}"#)]).await;
    let vercel = ProviderPreset::from_id("vercel-ai-gateway").unwrap();
    let models = discover(vercel, url, "key").await.unwrap();
    assert_eq!(models[0].context_length, Some(456));
    task.await.unwrap();
}

#[tokio::test]
async fn key_placement_and_quirks_come_from_the_registry_row() {
    let body = "data: [DONE]\n\n";
    // (configure, preset, required request text, forbidden request text)
    type Case = (
        fn(ProviderModel) -> ProviderModel,
        ProviderPreset,
        &'static [&'static str],
        &'static [&'static str],
    );
    let cases: Vec<Case> = vec![
        (
            |m| m,
            ProviderPreset::AzureOpenAiResponses,
            &["api-key: k\r\n"],
            &["authorization:"],
        ),
        (
            |m| m,
            ProviderPreset::CloudflareAiGateway,
            &["cf-aig-authorization: Bearer k\r\n"],
            &["x-api-key:"],
        ),
        (
            |m| m.protocol(Protocol::Anthropic),
            ProviderPreset::DatabricksUnityGateway,
            &["authorization: Bearer k\r\n"],
            &["x-api-key:"],
        ),
        (
            |m| m.user_agent("host/1"),
            ProviderPreset::KimiCoding,
            &["user-agent: orcacode\r\n"],
            &["host/1"],
        ),
        (
            |m| m.protocol(Protocol::Anthropic),
            ProviderPreset::OpenRouter,
            &["POST /messages "],
            &["/chat/completions"],
        ),
    ];
    for (configure, preset, present, absent) in cases {
        let (url, task) = serve(vec![json(body)]).await;
        let model = configure(ProviderModel::new(preset, "m").base_url(url).api_key("k"));
        // The fixture is not a valid stream; only the request is under test.
        let _ = model.generate(&Context::default(), &[]).await;
        let captured = task.await.unwrap().remove(0);
        let (request, lower) = (&captured.head, captured.lower());
        for header in present {
            assert!(
                request.contains(header) || lower.contains(&header.to_ascii_lowercase()),
                "{}: missing {header:?} in {request}",
                preset.id()
            );
        }
        for header in absent {
            assert!(
                !lower.contains(&header.to_ascii_lowercase()),
                "{}: unexpected {header:?} in {request}",
                preset.id()
            );
        }
    }
}
