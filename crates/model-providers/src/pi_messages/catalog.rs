//! Radius gateway.config.read, verified against https://radius.pi.dev/v1/openapi.json.
//! The API root includes `/v1`; no routing URLs from the response are consumed.

use crate::ModelInfo;
use orca_harness_core::ModelError;
use serde_json::Value;

pub(crate) async fn list_models(
    base_url: &str,
    key: Option<&str>,
) -> Result<Vec<ModelInfo>, ModelError> {
    parse_models(
        &crate::discovery::fetch_json(crate::discovery::get(base_url, "/config", key)).await?,
    )
}

fn parse_models(value: &Value) -> Result<Vec<ModelInfo>, ModelError> {
    crate::discovery::rows(value, "models")?
        .iter()
        .filter(|row| row["enabled"].as_bool() != Some(false))
        // The current schema has input modalities but no output modalities.
        // If output is advertised, exclude explicitly nontext models only.
        // Input images/audio do not imply nontext output.
        .filter(|row| {
            !row["output"].as_array().is_some_and(|output| {
                !output.is_empty()
                    && output.iter().all(Value::is_string)
                    && !output.iter().any(|kind| kind.as_str() == Some("text"))
            })
        })
        .map(|row| {
            let id = row["id"]
                .as_str()
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    ModelError::InvalidResponse("Radius catalog model has no nonempty ID".into())
                })?;
            Ok(ModelInfo {
                id: id.to_owned(),
                name: row["name"].as_str().map(str::to_owned),
                context_length: row["contextWindow"].as_u64().filter(|n| *n > 0),
                // Radius cost is USD per million tokens, not Pricing's per-token
                // units. Leave it unknown rather than passing through wrong units.
                pricing: None,
                // `reasoning` is a boolean, not an advertised effort selector.
                // `maxTokens` and `input` have no corresponding ModelInfo fields.
                reasoning: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn maps_only_advertised_metadata_without_id_heuristics() {
        let models = parse_models(&json!({
            "baseUrl": "https://untrusted.invalid/v1",
            "currency": "USD",
            "models": [{
                "id": "org/anything:virtual", "name": "Team model",
                "enabled": true, "contextWindow": 123456, "maxTokens": 8192,
                "reasoning": true, "input": ["text", "image"],
                "cost": {"input": 3, "output": 15, "cacheRead": 0.3, "cacheWrite": 3.75},
                "lab": "custom", "providers": []
            }, {"id": "unknown-audio-image-reasoning"},
            {"id": "disabled", "enabled": false},
            {"id": "image-only", "output": ["image"]},
            {"id": "mixed", "output": ["image", "text"]}]
        }))
        .unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "org/anything:virtual");
        assert_eq!(models[0].name.as_deref(), Some("Team model"));
        assert_eq!(models[0].context_length, Some(123456));
        assert!(models[0].pricing.is_none());
        assert!(models[0].reasoning.is_none());
        assert_eq!(models[1].id, "unknown-audio-image-reasoning");
        assert!(models[1].name.is_none());
        assert!(models[1].context_length.is_none());
        assert!(models[1].pricing.is_none());
        assert!(models[1].reasoning.is_none());
        assert_eq!(models[2].id, "mixed");
    }

    #[test]
    fn requires_models_and_ids_but_never_supplies_defaults() {
        for value in [
            json!({}),
            json!({"models": {}}),
            json!({"data": []}),
            json!({"models": [{}]}),
            json!({"models": [{"id": " "}]}),
        ] {
            assert!(parse_models(&value).is_err());
        }
        assert!(parse_models(&json!({"models": []})).unwrap().is_empty());
        let models = parse_models(&json!({"models": [{
            "id": "custom", "contextWindow": 0, "maxTokens": 4096
        }]}))
        .unwrap();
        assert!(models[0].context_length.is_none());
    }

    #[tokio::test]
    async fn public_and_bearer_requests_use_config_under_the_given_root() {
        for key in [None, Some("org-secret")] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/gateway/v1/", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buf = [0; 1024];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert_ne!(n, 0);
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert!(request.starts_with("get /gateway/v1/config http/1.1\r\n"));
                match key {
                    Some(_) => {
                        assert!(request.contains("\r\nauthorization: bearer org-secret\r\n"))
                    }
                    None => assert!(!request.contains("authorization:")),
                }
                let body = r#"{"models":[{"id":"organization/custom"}]}"#;
                socket.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                ).as_bytes()).await.unwrap();
            });
            let models = list_models(&base, key).await.unwrap();
            assert_eq!(models[0].id, "organization/custom");
            server.await.unwrap();
        }
    }
}
