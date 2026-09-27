use super::*;
use orca_harness_core::{Image, ModelDelta, ToolCall, ToolResult};
use serde_json::json;

#[test]
fn request_images_tools_and_signed_replay() {
    let model = GoogleModel::new("gemini-3-pro")
        .max_tokens(100)
        .thinking_level("high");
    let mut context = Context::new();
    context.push_system("system");
    context.push_user_with_images(
        "look",
        vec![Image {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        }],
    );
    let call = ToolCall {
        id: "call1".into(),
        name: "search".into(),
        arguments: json!({"q":"x"}),
    };
    context.push_assistant_tool_calls(None, vec![call.clone()]);
    context.append_tool_results(vec![ToolResult::ok(&call, json!({"ok":true}))]);
    let signatures: HashMap<String, Arc<[Value]>> = HashMap::from([(
        "call1".into(),
        Arc::from(vec![
            json!({"text":"thinking","thought":true,"thoughtSignature":"s"}),
            json!({"functionCall":{"name":"search","args":{"q":"x"}},"thoughtSignature":"sig"}),
        ]),
    )]);
    let body = model
        .body(
            &context,
            &[ToolSchema {
                name: "search".into(),
                description: "search".into(),
                parameters: json!({"type":"object"}),
            }],
            &signatures,
        )
        .unwrap();
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "system");
    assert_eq!(
        body["contents"][0]["parts"][1]["inlineData"]["data"],
        "AAAA"
    );
    assert_eq!(body["contents"][1]["parts"][0]["thoughtSignature"], "s");
    assert_eq!(body["contents"][1]["parts"][1]["thoughtSignature"], "sig");
    assert_eq!(
        body["contents"][2]["parts"][0]["functionResponse"]["name"],
        "search"
    );
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "high"
    );
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["type"],
        "object"
    );
}

#[test]
fn stream_deltas_usage_and_limits() {
    let mut state = stream::Accumulator::default();
    let deltas = state.apply(&json!({"candidates":[{"content":{"parts":[
        {"text":"reason","thought":true,"thoughtSignature":"abc"},
        {"text":"hello"}, {"functionCall":{"name":"run","args":{"x":1}},"thoughtSignature":"sig"}
    ]}}]}).to_string()).unwrap();
    assert!(matches!(&deltas[0], ModelDelta::Reasoning { text } if text == "reason"));
    assert!(matches!(&deltas[1], ModelDelta::Text { text } if text == "hello"));
    state.apply(&json!({"candidates":[{"finishReason":"STOP"}],"usageMetadata":{
        "promptTokenCount":20,"cachedContentTokenCount":7,"candidatesTokenCount":4,"thoughtsTokenCount":3
    }}).to_string()).unwrap();
    let collected = state.finish().unwrap();
    assert_eq!(collected.parts[1]["thoughtSignature"], "sig");
    let usage = collected.response.usage().unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.cache_read_tokens,
            usage.output_tokens,
            usage.reasoning_tokens
        ),
        (13, 7, 7, Some(3))
    );
    assert!(matches!(
        collected.response,
        ModelResponse::ToolCalls {
            content: Some(_),
            ..
        }
    ));
    let mut partial = stream::Accumulator::default();
    partial
        .apply(r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#)
        .unwrap();
    assert!(matches!(
        partial.finish(),
        Err(ModelError::IncompleteResponse { .. })
    ));
    let mut limited = stream::Accumulator::default();
    limited
        .apply(r#"{"candidates":[{"finishReason":"MAX_TOKENS"}]}"#)
        .unwrap();
    assert!(matches!(
        limited.finish(),
        Err(ModelError::OutputLimit { .. })
    ));
}

#[tokio::test]
async fn historical_signed_calls_survive_sequential_a_b_a_contexts() {
    let model = GoogleModel::new("gemini-test");
    let mut context = Context::new();
    context.push_user("start");
    for turn in 0..3 {
        let mut state = stream::Accumulator::default();
        let id = format!("upstream-{turn}");
        state.apply(&json!({"candidates":[{"content":{"parts":[
            {"text":format!("thinking {turn}"),"thought":true,"thoughtSignature":format!("thought-{turn}")},
            {"functionCall":{"id":id,"name":"search","args":{"turn":turn}},"thoughtSignature":format!("sig-{turn}")}
        ]},"finishReason":"STOP"}]}).to_string()).unwrap();
        let collected = state.finish().unwrap();
        model.update_signatures(&collected).await;
        let ModelResponse::ToolCalls { calls, .. } = collected.response else {
            panic!("expected calls")
        };
        assert_eq!(calls[0].id, id);
        context.push_assistant_tool_calls(None, calls.clone());
        context.append_tool_results(vec![ToolResult::ok(&calls[0], json!({"ok":true}))]);
        let body = model
            .body(&context, &[], &*model.signatures.lock().await)
            .unwrap();
        let mut seen = 0;
        for content in body["contents"].as_array().unwrap() {
            for part in content["parts"].as_array().unwrap() {
                if let Some(index) = part["functionCall"]["args"]["turn"].as_u64() {
                    assert_eq!(part["functionCall"]["id"], format!("upstream-{index}"));
                    assert_eq!(part["thoughtSignature"], format!("sig-{index}"));
                    seen += 1;
                }
                if let Some(id) = part["functionResponse"]["id"].as_str() {
                    assert!(id.starts_with("upstream-"));
                    assert_eq!(part["functionResponse"]["name"], "search");
                }
            }
        }
        assert_eq!(seen, turn + 1);
    }
    let mut compacted = Context::new();
    compacted.push_user("summary");
    let mut state = stream::Accumulator::default();
    state
        .apply(r#"{"candidates":[{"content":{"parts":[{"text":"done"}]},"finishReason":"STOP"}]}"#)
        .unwrap();
    model
        .body(&compacted, &[], &*model.signatures.lock().await)
        .unwrap();
    model.update_signatures(&state.finish().unwrap()).await;
    assert_eq!(model.signatures.lock().await.len(), 3);
    let body = model
        .body(&context, &[], &*model.signatures.lock().await)
        .unwrap();
    assert_eq!(
        body["contents"][1]["parts"][0]["thoughtSignature"],
        "thought-0"
    );
    assert_eq!(body["contents"][1]["parts"][1]["thoughtSignature"], "sig-0");
}

#[test]
fn sparse_cumulative_usage_and_blocked_prompt() {
    let mut state = stream::Accumulator::default();
    for meta in [
        json!({"promptTokenCount":20,"cachedContentTokenCount":5,"candidatesTokenCount":3,"thoughtsTokenCount":2}),
        json!({"candidatesTokenCount":4}),
        json!({"promptTokenCount":24}),
    ] {
        state
            .apply(&json!({"usageMetadata":meta}).to_string())
            .unwrap();
    }
    state
        .apply(r#"{"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#)
        .unwrap();
    let usage = *state.finish().unwrap().response.usage().unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.cache_read_tokens,
            usage.output_tokens,
            usage.reasoning_tokens
        ),
        (19, 5, 6, Some(2))
    );
    let mut blocked = stream::Accumulator::default();
    blocked
        .apply(r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#)
        .unwrap();
    assert!(matches!(
        blocked.finish(),
        Err(ModelError::ContentFiltered { .. })
    ));
}

#[tokio::test]
async fn wire_auth_stream_and_catalog() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&request).to_lowercase();
            assert!(text.contains("authorization: bearer token"));
            if text.contains("streamgeneratecontent") {
                assert!(text.contains("alt=sse"));
                let payload = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n\ndata: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n";
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{payload}", payload.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            } else {
                assert!(text.starts_with("get /v1beta/models "));
                let payload = r#"{"models":[{"name":"models/gemini-test","displayName":"Gemini Test","inputTokenLimit":1000,"supportedGenerationMethods":["generateContent"]}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{payload}",
                    payload.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        }
    });
    let model = GoogleModel::new("gemini-test")
        .base_url(format!("{url}/v1beta"))
        .bearer_token("token");
    let mut context = Context::new();
    context.push_user("hello");
    assert!(
        matches!(model.generate(&context, &[]).await.unwrap(), ModelResponse::Final { text, .. } if text == "hi")
    );
    let models = model.models().await.unwrap();
    assert_eq!(models[0].id, "gemini-test");
    assert_eq!(models[0].context_length, Some(1000));
    server.await.unwrap();
}

#[test]
fn thinking_configuration_does_not_infer_from_ids() {
    for id in [
        "arbitrary-id",
        "models/custom",
        "gemini-2.5-pro",
        "gemini-3-pro",
    ] {
        for budget in [-1, 0, 1234] {
            let body = GoogleModel::new(id)
                .thinking_budget(budget)
                .body(&Context::new(), &[], &HashMap::new())
                .unwrap();
            assert_eq!(
                body["generationConfig"]["thinkingConfig"],
                json!({"thinkingBudget":budget})
            );
        }
        let body = GoogleModel::new(id)
            .thinking_level("provider-level")
            .body(&Context::new(), &[], &HashMap::new())
            .unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({"thinkingLevel":"provider-level"})
        );
        let body = GoogleModel::new(id)
            .body(&Context::new(), &[], &HashMap::new())
            .unwrap();
        assert!(body.get("generationConfig").is_none());
        for effort in ["high", "low", "unknown", ""] {
            let error = GoogleModel::new(id)
                .reasoning_effort(effort)
                .body(&Context::new(), &[], &HashMap::new())
                .unwrap_err()
                .to_string();
            assert!(error.contains("unsupported"));
            assert!(error.contains("thinking_budget"));
            assert!(error.contains("thinking_level"));
        }
    }
    for model in [
        GoogleModel::new("x").thinking_budget(-2),
        GoogleModel::new("x").thinking_level(" "),
        GoogleModel::new("x")
            .thinking_budget(10)
            .thinking_level("high"),
        GoogleModel::new("x")
            .reasoning_effort("high")
            .thinking_budget(10),
    ] {
        assert!(model.body(&Context::new(), &[], &HashMap::new()).is_err());
    }
}

#[test]
fn discovery_validates_shape() {
    for value in [
        json!(null),
        json!([]),
        json!({}),
        json!({"publisherModels":[]}),
        json!({"models":null}),
        json!({"models":{}}),
        json!({"models":[],"nextPageToken":123}),
        json!({"models":[{}]}),
        json!({"models":[{"name":""}]}),
        json!({"models":[{"name":"x","supportedGenerationMethods":[42]}]}),
        json!({"models":[{"name":"x","inputTokenLimit":"100"}]}),
    ] {
        assert!(super::parse_models_page(value.clone()).is_err(), "{value}");
    }
    assert!(super::parse_models_page(json!({"models":[]}))
        .unwrap()
        .models
        .is_empty());
}

#[tokio::test]
async fn vertex_discovery_is_unavailable_without_network() {
    for root in [
        "http://127.0.0.1:1/v1/projects/p/locations/l/publishers/google",
        "https://us-central1-aiplatform.googleapis.com/v1",
    ] {
        let error = GoogleModel::new("anything")
            .base_url(root)
            .models()
            .await
            .unwrap_err();
        assert!(error.to_string().contains("discovery unavailable"));
    }
}

#[tokio::test]
async fn discovery_pages_filter_remote_methods_not_names() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for (path, payload) in [
            (
                "/models",
                json!({"models":[
                {"name":"models/unrelated-id","supportedGenerationMethods":["generateContent"],"displayName":"Remote","inputTokenLimit":123},
                {"name":"models/gemini-3-pro","supportedGenerationMethods":["embedContent"]},
                {"name":"models/no-methods"}],"nextPageToken":"page2"}),
            ),
            (
                "/models?pageToken=page2",
                json!({"models":[{"name":"models/another-id","supportedGenerationMethods":["generateContent"]}]}),
            ),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).starts_with(&format!("GET {path} HTTP/1.1")));
            let body = payload.to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let rows = GoogleModel::new("not-used")
        .base_url(root)
        .api_key("key")
        .models()
        .await
        .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["unrelated-id", "another-id"]
    );
    assert_eq!(rows[0].name.as_deref(), Some("Remote"));
    assert_eq!(rows[0].context_length, Some(123));
    assert!(rows.iter().all(|r| r.reasoning.is_none()));
    server.await.unwrap();
}

#[tokio::test]
async fn credential_bearing_calls_never_follow_redirects() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    for bearer in [false, true] {
        for catalog in [false, true] {
            for status in [301, 302, 303, 307, 308] {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}", listener.local_addr().unwrap());
                let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let location = format!("http://{}/leaked", destination.local_addr().unwrap());
                let server = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut buf = [0; 4096];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = socket.read(&mut buf).await.unwrap();
                        assert_ne!(n, 0);
                        request.extend_from_slice(&buf[..n]);
                    }
                    let request = String::from_utf8_lossy(&request).to_lowercase();
                    assert!(request.contains(if bearer {
                        "authorization: bearer secret"
                    } else {
                        "x-goog-api-key: secret"
                    }));
                    socket.write_all(format!("HTTP/1.1 {status} Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                });
                let model = GoogleModel::new("gemini-3-pro").base_url(url);
                let model = if bearer {
                    model.bearer_token("secret")
                } else {
                    model.api_key("secret")
                };
                if catalog {
                    assert!(model.models().await.is_err());
                } else {
                    assert!(model.generate(&Context::new(), &[]).await.is_err());
                }
                server.await.unwrap();
                assert!(tokio::time::timeout(
                    std::time::Duration::from_millis(20),
                    destination.accept()
                )
                .await
                .is_err());
            }
        }
    }
}
