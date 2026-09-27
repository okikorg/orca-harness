use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn exchange(
    status: &str,
    body: String,
    extra: &str,
) -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let response = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n{extra}\r\n{body}", body.len());
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut buf = [0; 1024];
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&buf[..n]);
            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        stream.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8(request).unwrap()
    });
    (url, task)
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

#[test]
fn routes_match_registry() {
    for model in ["claude-sonnet-4", "gpt-5", "gpt-4o", "unknown"] {
        let mut wrapper = CopilotModel::new(model)
            .max_tokens(123)
            .reasoning_effort("high");
        let protocol = match wrapper.state.get_mut().adapter.as_ref().unwrap() {
            Adapter::Anthropic(_) => Protocol::Anthropic,
            Adapter::Responses(_) => Protocol::Responses,
            Adapter::Chat(_) => Protocol::ChatCompletions,
        };
        assert_eq!(
            protocol,
            ProviderPreset::GithubCopilot.route(model).protocol
        );
    }
}

#[tokio::test]
async fn exchanges_caches_and_refreshes() {
    let (url, task) = exchange(
        "200 OK",
        credential(now() + 3600, ProviderPreset::GithubCopilot.base_url()),
        "",
    )
    .await;
    let mut wrapper = CopilotModel::new("gpt-4o").api_key("github-secret");
    wrapper.token_url = Some(url);
    {
        let mut state = wrapper.state.lock().await;
        wrapper.refresh(&mut state).await.unwrap();
        let request = task.await.unwrap().to_lowercase();
        assert!(request.contains("authorization: bearer github-secret"));
        assert!(request.contains("copilot-integration-id: vscode-chat"));
        // The listener is gone: another exchange would fail.
        wrapper.refresh(&mut state).await.unwrap();
        state.expires_at = 0;
    }
    let (url, task) = exchange(
        "200 OK",
        credential(now() + 7200, ProviderPreset::GithubCopilot.base_url()),
        "",
    )
    .await;
    wrapper.token_url = Some(url);
    let mut state = wrapper.state.lock().await;
    wrapper.refresh(&mut state).await.unwrap();
    assert!(state.expires_at > now() + 7000);
    assert!(state.adapter.is_some());
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
        let (url, task) = exchange("200 OK", body, "").await;
        let mut wrapper = CopilotModel::new("gpt-4o").api_key("github-secret");
        wrapper.token_url = Some(url);
        let mut state = wrapper.state.lock().await;
        let err = wrapper.refresh(&mut state).await.unwrap_err().to_string();
        assert!(!err.contains("copilot-secret") && !err.contains("github-secret"));
        assert_eq!(state.expires_at, 0);
        assert!(state.adapter.is_some());
        task.await.unwrap();
    }
}

#[tokio::test]
async fn exchange_does_not_follow_even_same_origin_redirect() {
    // Redirecting to the now-closed same-origin listener would be a transport
    // failure if followed. Instead the exchange must report the 302 itself.
    let (url, task) = exchange("302 Found", "github-secret".into(), "Location: /steal\r\n").await;
    let mut wrapper = CopilotModel::new("gpt-4o").api_key("github-secret");
    wrapper.token_url = Some(url);
    let mut state = wrapper.state.lock().await;
    let err = wrapper.refresh(&mut state).await.unwrap_err().to_string();
    assert!(err.contains("302"));
    assert!(!err.contains("github-secret"));
    task.await.unwrap();
}

#[tokio::test]
async fn missing_token_fails_locally() {
    let wrapper = CopilotModel::new("gpt-4o");
    assert!(wrapper
        .refresh(&mut *wrapper.state.lock().await)
        .await
        .is_err());
}
