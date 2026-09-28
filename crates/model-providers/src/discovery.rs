//! Shared model-discovery transport. Each provider keeps only its parser.
use crate::ModelInfo;
use orca_harness_core::ModelError;
use serde_json::Value;

/// `GET {root}{path}` with optional Bearer auth.
pub(crate) fn get(root: &str, path: &str, key: Option<&str>) -> reqwest::RequestBuilder {
    let request = crate::http::client().get(format!("{}{path}", root.trim_end_matches('/')));
    match key {
        Some(key) => request.bearer_auth(key),
        None => request,
    }
}

/// Send a catalog request and decode its JSON body. Rejected credentials are
/// `Authentication`; other failures keep the provider's status and body.
pub(crate) async fn fetch_json(request: reqwest::RequestBuilder) -> Result<Value, ModelError> {
    fetch(request, "model discovery", false).await
}

/// Send a JSON request, naming the operation `label` in failures. With
/// `redact`, failures never echo the response body, for services whose error
/// bodies may reflect exchanged credentials.
pub(crate) async fn fetch(
    request: reqwest::RequestBuilder,
    label: &str,
    redact: bool,
) -> Result<Value, ModelError> {
    let response = request
        .timeout(crate::http::CATALOG_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            if redact {
                ModelError::Request(format!("{label} transport failed"))
            } else {
                crate::http_error::transport_error(&e)
            }
        })?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err(ModelError::Authentication(format!(
            "{label} rejected credentials (HTTP {status})"
        )));
    }
    if redact && !status.is_success() {
        return Err(ModelError::Request(format!(
            "{label} failed (HTTP {status})"
        )));
    }
    crate::http_error::check_response(response)
        .await?
        .json()
        .await
        .map_err(|e| ModelError::InvalidResponse(e.to_string()))
}

/// The rows of a named array field.
pub(crate) fn rows<'a>(value: &'a Value, field: &str) -> Result<&'a [Value], ModelError> {
    value[field]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| ModelError::InvalidResponse(format!("model catalog has no {field} array")))
}

/// Parse the OpenAI Models interface (`{"data": [...]}`). Only metadata is
/// read, never routing URLs.
pub(crate) fn openai_models(value: &Value) -> Result<Vec<ModelInfo>, ModelError> {
    rows(value, "data")?.iter().map(openai_model).collect()
}

pub(crate) fn openai_model(row: &Value) -> Result<ModelInfo, ModelError> {
    let model: ModelInfo = serde_json::from_value(row.clone())
        .map_err(|e| ModelError::InvalidResponse(e.to_string()))?;
    if model.id.trim().is_empty() {
        return Err(ModelError::InvalidResponse(
            "model discovery returned an empty ID".into(),
        ));
    }
    Ok(model)
}

/// List an OpenAI-compatible endpoint's models.
pub(crate) async fn list_openai_models(
    base_url: &str,
    key: Option<&str>,
) -> Result<Vec<ModelInfo>, ModelError> {
    openai_models(&fetch_json(get(base_url, "/models", key)).await?)
}

/// Ollama's native `/api/show`: prefer an explicit `num_ctx` parameter (the
/// serving window) over the model's trained maximum (`*.context_length`).
pub(crate) async fn ollama_context_window(base_url: &str, model: &str) -> Option<u64> {
    let host = base_url.trim_end_matches('/').trim_end_matches("/v1");
    let body: Value = crate::http::client()
        .post(format!("{host}/api/show"))
        .timeout(std::time::Duration::from_secs(5))
        .json(&serde_json::json!({"model": model}))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let num_ctx = body["parameters"].as_str().and_then(|params| {
        params.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            (parts.next() == Some("num_ctx")).then(|| parts.next()?.parse().ok())?
        })
    });
    num_ctx.or_else(|| {
        body["model_info"]
            .as_object()?
            .iter()
            .find_map(|(key, value)| key.ends_with(".context_length").then(|| value.as_u64())?)
    })
}
