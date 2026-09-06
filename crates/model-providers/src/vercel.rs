//! Vercel AI Gateway conventions layered over OpenAI Chat Completions.

use serde_json::Value;

use crate::{ModelInfo, ReasoningCapabilities, SupportedEfforts};
use orca_harness_core::ModelError;

pub const VERCEL_GATEWAY_BASE_URL: &str = "https://ai-gateway.vercel.sh/v1";

/// Fetch Vercel's catalog and map its `context_window` and
/// `reasoning_options` fields into the provider-neutral model metadata.
pub async fn list_models(api_key: Option<&str>) -> Result<Vec<ModelInfo>, ModelError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| ModelError::Request(error.to_string()))?;
    let mut request = client.get(format!("{VERCEL_GATEWAY_BASE_URL}/models"));
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }
    let response = request
        .send()
        .await
        .map_err(|error| ModelError::Request(error.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| ModelError::Request(error.to_string()))?;
    if !status.is_success() {
        return Err(ModelError::Request(format!("HTTP {status}: {body}")));
    }
    parse_models(&body)
}

fn parse_models(body: &str) -> Result<Vec<ModelInfo>, ModelError> {
    let listing: Value = serde_json::from_str(body)
        .map_err(|error| ModelError::InvalidResponse(format!("{error}: {body}")))?;
    let entries = listing["data"]
        .as_array()
        .ok_or_else(|| ModelError::InvalidResponse("model catalog has no data array".into()))?;
    let models = entries
        .iter()
        .map(|entry| {
            let mut model: ModelInfo = serde_json::from_value(entry.clone())
                .map_err(|error| ModelError::InvalidResponse(error.to_string()))?;
            model.context_length = entry["context_window"].as_u64();
            model.reasoning = effort_values(entry).map(|efforts| ReasoningCapabilities {
                supported_efforts: Some(SupportedEfforts::Listed(efforts)),
                default_effort: None,
            });
            Ok(model)
        })
        .collect::<Result<Vec<_>, ModelError>>()?;
    Ok(models)
}

fn effort_values(model: &Value) -> Option<Vec<String>> {
    model["reasoning_options"]
        .as_array()?
        .iter()
        .find_map(|option| {
            (option["type"] == "effort").then(|| {
                option["values"]
                    .as_array()?
                    .iter()
                    .map(|value| value.as_str().map(str::to_owned))
                    .collect()
            })?
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_vercel_catalog_fields() {
        let models = parse_models(
            r#"{"data":[{"id":"openai/gpt-5","context_window":400000,"reasoning_options":[{"type":"effort","values":["low","medium","high"]}]}]}"#,
        )
        .unwrap();
        assert_eq!(models[0].context_length, Some(400_000));
        assert_eq!(
            models[0].reasoning.as_ref().unwrap().supported_efforts,
            Some(SupportedEfforts::Listed(vec![
                "low".into(),
                "medium".into(),
                "high".into()
            ]))
        );
    }

    #[test]
    fn toggle_only_reasoning_does_not_invent_effort_choices() {
        let models = parse_models(
            r#"{"data":[{"id":"alibaba/qwen-3-14b","context_window":40960,"reasoning_options":[{"type":"toggle"}]}]}"#,
        )
        .unwrap();
        assert!(models[0].reasoning.is_none());
    }
}
