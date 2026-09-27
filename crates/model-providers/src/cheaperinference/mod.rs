//! CheaperInference conventions layered over OpenAI Chat Completions.

use serde_json::Value;

use crate::{ModelInfo, Pricing};
use orca_harness_core::ModelError;

pub const CHEAPERINFERENCE_BASE_URL: &str = "https://api.cheaperinference.com/v1";

/// Fetch CheaperInference's advertised catalog and map its per-million
/// prices into the provider-neutral per-token metadata. The catalog lives at
/// `/public/models` on the API host, outside the OpenAI-compatible `/v1`
/// surface, and needs no credentials.
pub async fn list_models(base_url: &str) -> Result<Vec<ModelInfo>, ModelError> {
    let url = reqwest::Url::parse(base_url)
        .and_then(|root| root.join("/public/models"))
        .map_err(|e| ModelError::Request(format!("invalid CheaperInference base URL: {e}")))?;
    parse(&crate::discovery::fetch_json(crate::http::client().get(url)).await?)
}

fn parse(listing: &Value) -> Result<Vec<ModelInfo>, ModelError> {
    let models = crate::discovery::rows(listing, "models")?
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

    fn parse_models(body: &str) -> Result<Vec<ModelInfo>, ModelError> {
        parse(&serde_json::from_str(body).unwrap())
    }

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
