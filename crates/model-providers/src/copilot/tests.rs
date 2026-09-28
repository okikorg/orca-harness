use super::*;
use crate::test_server::{json as ok, serve, status, Captured, Reply};
use serde_json::json;
use tokio::task::JoinHandle;

/// Serve one reply.
async fn exchange(reply: Reply) -> (String, JoinHandle<Vec<Captured>>) {
    serve(vec![reply]).await
}

fn credential(expires_at: u64, endpoint: &str) -> String {
    json!({"token":"copilot-secret", "expires_at":expires_at,
        "endpoints":{"api":endpoint}})
    .to_string()
}

#[test]
fn endpoints_are_strictly_validated() {
    for endpoint in [
        "https://api.individual.githubcopilot.com",
        "https://api.business.githubcopilot.com/",
    ] {
        assert!(validate_endpoint(endpoint).is_ok());
    }
    for endpoint in [
        "http://api.githubcopilot.com",
        "https://githubcopilot.com",
        "https://evilgithubcopilot.com",
        "https://api.githubcopilot.com.evil.test",
        "https://user@api.githubcopilot.com",
        "https://api.githubcopilot.com:444",
        "https://api.githubcopilot.com?token=x",
        "https://api.githubcopilot.com/#x",
        "https://127.0.0.1",
        "not a url",
    ] {
        assert!(validate_endpoint(endpoint).is_err(), "{endpoint}");
    }
}

#[tokio::test]
async fn exchanges_caches_and_refreshes() {
    let (url, task) = exchange(ok(credential(
        now() + 3600,
        ProviderPreset::GithubCopilot.base_url(),
    )))
    .await;
    let mut wrapper = CopilotModel::new("gpt-4o").api_key("github-secret");
    wrapper.token_url = Some(url);
    {
        let mut state = wrapper.state.lock().await;
        wrapper.refresh(&mut state).await.unwrap();
        let request = task.await.unwrap()[0].lower();
        assert!(request.contains("authorization: bearer github-secret"));
        assert!(request.contains("copilot-integration-id: vscode-chat"));
        // The listener is gone: another exchange would fail.
        wrapper.refresh(&mut state).await.unwrap();
        state.expires_at = 0;
    }
    let (url, task) = exchange(ok(credential(
        now() + 7200,
        ProviderPreset::GithubCopilot.base_url(),
    )))
    .await;
    wrapper.token_url = Some(url);
    let mut state = wrapper.state.lock().await;
    wrapper.refresh(&mut state).await.unwrap();
    assert!(state.expires_at > now() + 7000);
    assert!(state.adapter.is_none());
    task.await.unwrap();
}

#[tokio::test]
async fn invalid_responses_leave_state_intact_and_do_not_disclose_secrets() {
    for body in [
        credential(now() + 3600, "https://evil.test"),
        credential(now() - 1, ProviderPreset::GithubCopilot.base_url()),
        json!({"token":"copilot-secret"}).to_string(),
        "copilot-secret invalid json".into(),
    ] {
        let (url, task) = exchange(ok(body)).await;
        let mut wrapper = CopilotModel::new("gpt-4o").api_key("github-secret");
        wrapper.token_url = Some(url);
        let mut state = wrapper.state.lock().await;
        let err = wrapper.refresh(&mut state).await.unwrap_err().to_string();
        assert!(!err.contains("copilot-secret") && !err.contains("github-secret"));
        assert_eq!(state.expires_at, 0);
        assert!(state.adapter.is_none());
        task.await.unwrap();
    }
}

#[tokio::test]
async fn exchange_failures_are_redacted() {
    // Redirects are not followed (the shared client's policy); the exchange
    // reports each failing status without echoing the body.
    for (reply, expected) in [
        (
            status("302 Found", "github-secret").header("Location", "/steal"),
            "302",
        ),
        (status("401 Unauthorized", "github-secret"), "rejected"),
        (status("500 Error", "github-secret"), "500"),
    ] {
        let (url, task) = exchange(reply).await;
        let mut wrapper = CopilotModel::new("gpt-4o").api_key("github-secret");
        wrapper.token_url = Some(url);
        let mut state = wrapper.state.lock().await;
        let err = wrapper.refresh(&mut state).await.unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("Copilot token exchange") && text.contains(expected),
            "{text}"
        );
        assert!(!text.contains("github-secret"));
        assert_eq!(
            matches!(err, ModelError::Authentication(_)),
            expected == "rejected"
        );
        task.await.unwrap();
    }
}

#[tokio::test]
async fn missing_token_fails_locally() {
    let wrapper = CopilotModel::new("gpt-4o");
    assert!(wrapper
        .refresh(&mut *wrapper.state.lock().await)
        .await
        .is_err());
}

async fn with_catalog(model: &str, body: String) -> (CopilotModel, JoinHandle<Vec<Captured>>) {
    let (api, catalog_task) = exchange(ok(body)).await;
    let (token_url, token_task) = exchange(ok(credential(
        now() + 3600,
        ProviderPreset::GithubCopilot.base_url(),
    )))
    .await;
    let mut wrapper = CopilotModel::new(model)
        .api_key("github-secret")
        .max_tokens(123)
        .reasoning_effort("high");
    wrapper.token_url = Some(token_url);
    wrapper.api_fixture = Some(api);
    wrapper
        .refresh(&mut *wrapper.state.lock().await)
        .await
        .unwrap();
    let request = token_task.await.unwrap()[0].head.clone();
    assert!(request.contains("github-secret"));
    assert!(!request.contains("copilot-secret"));
    (wrapper, catalog_task)
}

#[tokio::test]
async fn arbitrary_ids_select_advertised_protocols_and_survive_refresh() {
    for (id, endpoint) in [
        ("opaque-a", "/v1/messages"),
        ("opaque-b", "/responses"),
        ("opaque-c", "/chat/completions"),
    ] {
        let (mut wrapper, task) = with_catalog(
            id,
            json!({"data":[
                {"id":"opaque-a", "supported_endpoints":["/v1/messages"]},
                {"id":"opaque-b", "supported_endpoints":["/responses"]},
                {"id":"opaque-c", "supported_endpoints":["/chat/completions"]}
            ]})
            .to_string(),
        )
        .await;
        {
            let mut state = wrapper.state.lock().await;
            wrapper.prepare(&mut state).await.unwrap();
            let matches = matches!(
                (endpoint, state.adapter.as_ref().unwrap()),
                ("/v1/messages", Adapter::Anthropic(_))
                    | ("/responses", Adapter::Responses(_))
                    | ("/chat/completions", Adapter::Chat(_))
            );
            assert!(matches);
            // Neither exchange nor catalog listeners remain: selection is cached.
            wrapper.prepare(&mut state).await.unwrap();
            state.expires_at = 0;
        }
        let request = task.await.unwrap()[0].lower();
        assert!(request.starts_with("get /models "));
        assert!(request.contains("authorization: bearer copilot-secret"));
        assert!(!request.contains("github-secret"));
        let (url, task) = exchange(ok(credential(
            now() + 7200,
            ProviderPreset::GithubCopilot.base_url(),
        )))
        .await;
        wrapper.token_url = Some(url);
        let mut state = wrapper.state.lock().await;
        let before = std::mem::discriminant(state.adapter.as_ref().unwrap());
        wrapper.prepare(&mut state).await.unwrap();
        assert_eq!(
            before,
            std::mem::discriminant(state.adapter.as_ref().unwrap())
        );
        task.await.unwrap();
    }
}

#[tokio::test]
async fn metadata_is_advertised_not_inferred() {
    let (wrapper, task) = with_catalog(
        "unused",
        json!({"data":[
            {"id":"claude-unknown", "future_metadata":true},
            {"id":"arbitrary", "name":"Display", "capabilities":{
                "limits":{"max_context_window_tokens":12345},
                "supports":{"reasoning_effort":["low","high"]}
            }}
        ]})
        .to_string(),
    )
    .await;
    let models = wrapper.models().await.unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "claude-unknown");
    assert!(models[0].name.is_none());
    assert!(models[0].context_length.is_none());
    assert!(models[0].pricing.is_none());
    assert!(models[0].reasoning.is_none());
    assert_eq!(models[1].context_length, Some(12345));
    assert_eq!(models[1].name.as_deref(), Some("Display"));
    assert_eq!(
        models[1].reasoning.as_ref().unwrap().supported_efforts,
        Some(SupportedEfforts::Listed(vec!["low".into(), "high".into()]))
    );
    task.await.unwrap();
}

#[tokio::test]
async fn unknown_model_and_missing_or_unknown_capabilities_fail_actionably() {
    for (id, entry, expected) in [
        (
            "absent",
            json!({"id":"other", "supported_endpoints":["/responses"]}),
            "models()",
        ),
        ("opaque", json!({"id":"opaque"}), "endpoint capabilities"),
        (
            "opaque",
            json!({"id":"opaque", "supported_endpoints":["ws:/responses", "/future"]}),
            "endpoint capabilities",
        ),
    ] {
        let (wrapper, task) = with_catalog(id, json!({"data":[entry]}).to_string()).await;
        let mut state = wrapper.state.lock().await;
        let err = wrapper.prepare(&mut state).await.unwrap_err().to_string();
        assert!(err.contains(expected), "{err}");
        assert!(state.adapter.is_none());
        task.await.unwrap();
    }
}

#[tokio::test]
async fn discovery_errors_are_redacted_and_redirects_not_followed() {
    for (reply, expected) in [
        (
            status("302 Found", "copilot-secret github-secret").header("Location", "/steal"),
            "302",
        ),
        (status("500 Error", "copilot-secret github-secret"), "500"),
        (
            ok("copilot-secret github-secret"),
            "Invalid Copilot model catalog",
        ),
    ] {
        let (mut wrapper, initial) = with_catalog("opaque", "{\"data\":[]}".into()).await;
        // Replace fixture without ever sending a request to the initial catalog.
        initial.abort();
        let (api, task) = exchange(reply).await;
        wrapper.state.get_mut().credential.as_mut().unwrap().1 = api;
        let err = wrapper.models().await.unwrap_err().to_string();
        assert!(err.contains(expected), "{err}");
        assert!(!err.contains("copilot-secret") && !err.contains("github-secret"));
        assert!(wrapper.state.get_mut().catalog.is_none());
        let request = task.await.unwrap()[0].head.clone();
        assert!(!request.contains("github-secret"));
    }
}
