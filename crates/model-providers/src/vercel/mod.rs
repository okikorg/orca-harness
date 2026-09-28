//! Vercel AI Gateway conventions layered over OpenAI Chat Completions.

use serde_json::Value;

use crate::{ModelInfo, ReasoningCapabilities, SupportedEfforts};
use orca_harness_core::ModelError;

pub const VERCEL_GATEWAY_BASE_URL: &str = "https://ai-gateway.vercel.sh/v1";

/// Fetch Vercel's catalog and map its `context_window` and
/// `reasoning_options` fields into the provider-neutral model metadata.
pub async fn list_models(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<ModelInfo>, ModelError> {
    parse(&crate::discovery::fetch_json(crate::discovery::get(base_url, "/models", api_key)).await?)
}

fn parse(listing: &Value) -> Result<Vec<ModelInfo>, ModelError> {
    crate::discovery::rows(listing, "data")?
        .iter()
        .map(|entry| {
            let mut model = crate::discovery::openai_model(entry)?;
            model.context_length = entry["context_window"].as_u64();
            model.reasoning = effort_values(entry).map(|efforts| ReasoningCapabilities {
                supported_efforts: Some(SupportedEfforts::Listed(efforts)),
                default_effort: None,
            });
            Ok(model)
        })
        .collect()
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

    fn parse_models(body: &str) -> Result<Vec<ModelInfo>, ModelError> {
        parse(&serde_json::from_str(body).unwrap())
    }

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
