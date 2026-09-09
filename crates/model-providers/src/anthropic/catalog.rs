use std::collections::HashSet;

use orca_harness_core::ModelError;
use serde::Deserialize;
use serde_json::Value;

use crate::{ModelInfo, ReasoningCapabilities, SupportedEfforts};

#[derive(Deserialize)]
struct Page {
    data: Vec<Row>,
    has_more: bool,
    last_id: Option<String>,
}

#[derive(Deserialize)]
struct Row {
    id: String,
    display_name: Option<String>,
    max_input_tokens: Option<u64>,
    #[serde(default)]
    capabilities: Value,
}

impl From<Row> for ModelInfo {
    fn from(row: Row) -> Self {
        let effort = &row.capabilities["effort"];
        let supported = ["low", "medium", "high", "xhigh", "max"]
            .into_iter()
            .filter(|level| effort[*level]["supported"] == true)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        Self {
            id: row.id,
            name: row.display_name,
            context_length: row.max_input_tokens.filter(|count| *count > 0),
            pricing: None,
            reasoning: (!supported.is_empty()).then_some(ReasoningCapabilities {
                supported_efforts: Some(SupportedEfforts::Listed(supported)),
                default_effort: None,
            }),
        }
    }
}

/// Fetch every page of the native Models API using API-key/version headers.
pub async fn list_models(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<ModelInfo>, ModelError> {
    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    loop {
        let mut request = super::authenticated_request(
            &format!("{}/models", base_url.trim_end_matches('/')),
            api_key,
            reqwest::Method::GET,
        )?
        .timeout(crate::http::CATALOG_TIMEOUT)
        .query(&[("limit", "1000")]);
        if let Some(cursor) = &cursor {
            request = request.query(&[("after_id", cursor)]);
        }
        let response = request
            .send()
            .await
            .map_err(|error| crate::http_error::transport_error(&error))?;
        let response = crate::http_error::check_response(response).await?;
        let page: Page = response
            .json()
            .await
            .map_err(|error| ModelError::InvalidResponse(error.to_string()))?;
        let next = next_cursor(&page, &mut seen)?;
        models.extend(page.data.into_iter().map(ModelInfo::from));
        match next {
            Some(next) => cursor = Some(next),
            None => return Ok(models),
        }
    }
}

/// Resolve one model by id. `/models/{id}` accepts aliases (`claude-haiku-4-5`)
/// as well as the dated ids the catalog lists, so this answers the context
/// window for a configured model that never appears verbatim in `list_models`.
pub async fn retrieve_model(
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
) -> Result<ModelInfo, ModelError> {
    // Model ids are unreserved URL characters; anything else is not an id we
    // could resolve, and interpolating it would rewrite the request path.
    if model.is_empty()
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte))
    {
        return Err(ModelError::Request(format!(
            "not a valid Anthropic model id: {model:?}"
        )));
    }
    let response = super::authenticated_request(
        &format!("{}/models/{}", base_url.trim_end_matches('/'), model),
        api_key,
        reqwest::Method::GET,
    )?
    .timeout(crate::http::CATALOG_TIMEOUT)
    .send()
    .await
    .map_err(|error| crate::http_error::transport_error(&error))?;
    let response = crate::http_error::check_response(response).await?;
    let row: Row = response
        .json()
        .await
        .map_err(|error| ModelError::InvalidResponse(error.to_string()))?;
    Ok(row.into())
}

fn next_cursor(page: &Page, seen: &mut HashSet<String>) -> Result<Option<String>, ModelError> {
    if !page.has_more {
        return Ok(None);
    }
    let cursor = page
        .last_id
        .as_ref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            ModelError::InvalidResponse("Anthropic catalog missing pagination cursor".into())
        })?;
    if !seen.insert(cursor.clone()) {
        return Err(ModelError::InvalidResponse(
            "Anthropic catalog repeated pagination cursor".into(),
        ));
    }
    Ok(Some(cursor.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_catalog_metadata_and_pagination() {
        let page: Page = serde_json::from_value(json!({
            "data": [{"id":"claude-test", "display_name":"Claude Test", "max_input_tokens":200000,
                "capabilities":{"effort":{"supported":true,"low":{"supported":true},"high":{"supported":true}}}}],
            "has_more": true, "last_id":"claude-test"
        })).unwrap();
        let mut seen = HashSet::new();
        assert_eq!(
            next_cursor(&page, &mut seen).unwrap().as_deref(),
            Some("claude-test")
        );
        assert!(next_cursor(&page, &mut seen).is_err());
        let model: ModelInfo = page.data.into_iter().next().unwrap().into();
        assert_eq!(model.name.as_deref(), Some("Claude Test"));
        assert_eq!(model.context_length, Some(200000));
        assert_eq!(
            model.reasoning.unwrap().supported_efforts,
            Some(SupportedEfforts::Listed(vec!["low".into(), "high".into()]))
        );
        let legacy: Row =
            serde_json::from_value(json!({"id":"legacy","display_name":"Legacy"})).unwrap();
        assert!(ModelInfo::from(legacy).reasoning.is_none());
        let bad: Page =
            serde_json::from_value(json!({"data":[],"has_more":true,"last_id":null})).unwrap();
        assert!(next_cursor(&bad, &mut seen).is_err());
    }

    #[tokio::test]
    async fn retrieve_parses_a_bare_model_row_and_rejects_unusable_ids() {
        // `/models/{id}` returns the model object itself, not a page.
        let row: Row = serde_json::from_value(json!({
            "id":"claude-haiku-4-5-20251001","display_name":"Claude Haiku 4.5",
            "max_input_tokens":200000
        }))
        .unwrap();
        assert_eq!(ModelInfo::from(row).context_length, Some(200000));
        // A model with no window reported stays unknown rather than reading 0.
        let absent: Row = serde_json::from_value(json!({"id":"x","max_input_tokens":0})).unwrap();
        assert_eq!(ModelInfo::from(absent).context_length, None);
        for id in ["", "../models", "a/b", "claude 5"] {
            let error = retrieve_model("https://x/v1", Some("k"), id).await;
            assert!(
                matches!(error, Err(ModelError::Request(_))),
                "{id:?} was not rejected before the request"
            );
        }
    }
}
