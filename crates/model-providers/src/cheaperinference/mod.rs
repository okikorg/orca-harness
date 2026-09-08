//! CheaperInference conventions layered over OpenAI Chat Completions.

use serde_json::Value;

use crate::{ModelInfo, Pricing};
use orca_harness_core::ModelError;

pub const CHEAPERINFERENCE_BASE_URL: &str = "https://api.cheaperinference.com/v1";

/// The catalog lives outside the OpenAI-compatible `/v1` surface and needs
/// no credentials, so browsing works before a key is configured.
const CHEAPERINFERENCE_MODELS_URL: &str = "https://api.cheaperinference.com/public/models";

/// Fetch CheaperInference's advertised catalog and map its per-million
/// prices into the provider-neutral per-token metadata.
pub async fn list_models() -> Result<Vec<ModelInfo>, ModelError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| crate::http_error::transport_error(&error))?;
    let response = client
        .get(CHEAPERINFERENCE_MODELS_URL)
        .send()
        .await
        .map_err(|error| crate::http_error::transport_error(&error))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| crate::http_error::transport_error(&error))?;
    if !status.is_success() {
        return Err(ModelError::Request(format!("HTTP {status}: {body}")));
    }
    parse_models(&body)
}

fn parse_models(body: &str) -> Result<Vec<ModelInfo>, ModelError> {
    let listing: Value = serde_json::from_str(body)
        .map_err(|error| ModelError::InvalidResponse(format!("{error}: {body}")))?;
    let entries = listing["models"]
        .as_array()
        .ok_or_else(|| ModelError::InvalidResponse("model catalog has no models array".into()))?;
    let models = entries
        .iter()
        .filter(|entry| {
            entry["model_type"] == "text" && entry["is_visible"].as_bool().unwrap_or(true)
        })
        .map(|entry| ModelInfo {
            id: entry["id"].as_str().unwrap_or_default().to_owned(),
            name: None,
            context_length: entry["context_length"].as_u64(),
            pricing: Some(Pricing {
                prompt: per_token(&entry["input_per_million"]),
                completion: per_token(&entry["output_per_million"]),
            }),
            // The catalog advertises reasoning support without enumerating
            // effort values, so no effort selector is invented here.
            reasoning: None,
        })
        .filter(|model| !model.id.is_empty())
        .collect();
    Ok(models)
}

fn per_token(per_million: &Value) -> Option<String> {
    let price: f64 = per_million.as_str()?.trim().parse().ok()?;
    Some((price / 1e6).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &str = r#"{"models":[
        {"id":"claude-opus-5","context_length":1000000,"model_type":"text","is_visible":true,
         "input_per_million":"3.500000","output_per_million":"17.500000","supports_reasoning":true},
        {"id":"some-image-model","context_length":8192,"model_type":"image","is_visible":true,
         "input_per_million":"1.000000","output_per_million":"2.000000"}
    ]}"#;

    #[test]
    fn maps_per_million_prices_to_per_token() {
        let models = parse_models(CATALOG).unwrap();
        assert_eq!(models[0].id, "claude-opus-5");
        assert_eq!(models[0].context_length, Some(1_000_000));
        assert_eq!(
            models[0].summary(),
            "claude-opus-5  1M ctx  $3.50/M in $17.50/M out"
        );
    }

    #[test]
    fn keeps_only_visible_text_models() {
        let models = parse_models(CATALOG).unwrap();
        assert_eq!(models.len(), 1);
    }

    #[test]
    fn advertised_reasoning_does_not_invent_effort_choices() {
        let models = parse_models(CATALOG).unwrap();
        assert!(models[0].reasoning.is_none());
    }
}
