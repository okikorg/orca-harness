//! OpenRouter adapter for Orca Harness.
//!
//! OpenRouter speaks the OpenAI chat-completions protocol, so generation
//! delegates to the OpenAI adapter pointed at openrouter.ai. This crate
//! adds what the generic adapter deliberately leaves out: the model
//! catalog (`GET /models`, with OpenRouter's context and pricing
//! metadata), attribution headers, and the endpoint conventions.

use async_trait::async_trait;
use serde::Deserialize;

pub use crate::catalog::{ModelInfo, Pricing};
use crate::openai::OpenAiModel;
use crate::SupportedEfforts;
use orca_harness_core::{Context, DeltaSink, Model, ModelError, ModelResponse, ToolSchema};

pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
const ALL_REASONING_EFFORTS: [&str; 7] =
    ["max", "xhigh", "high", "medium", "low", "minimal", "none"];

/// An OpenRouter-hosted model. Construction presets the endpoint; use
/// [`base_url`](Self::base_url) only to point tests elsewhere.
pub struct OpenRouterModel {
    inner: OpenAiModel,
    base_url: String,
    api_key: Option<String>,
}

impl OpenRouterModel {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            inner: OpenAiModel::new(model)
                .base_url(OPENROUTER_BASE_URL)
                .usage_accounting(true),
            base_url: OPENROUTER_BASE_URL.into(),
            api_key: None,
        }
    }

    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        self.inner = self.inner.base_url(base_url.clone());
        self.base_url = base_url;
        self
    }

    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        let api_key = api_key.into();
        self.inner = self.inner.api_key(api_key.clone());
        self.api_key = Some(api_key);
        self
    }

    /// Attribution: the site OpenRouter credits with this traffic.
    pub fn referer(mut self, referer: impl Into<String>) -> Self {
        self.inner = self.inner.header("HTTP-Referer", referer);
        self
    }

    /// Attribution: the app name shown on OpenRouter dashboards.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.inner = self.inner.header("X-OpenRouter-Title", title);
        self
    }

    /// Attribution: OpenRouter marketplace categories for this app.
    pub fn categories(mut self, categories: impl Into<String>) -> Self {
        self.inner = self.inner.header("X-OpenRouter-Categories", categories);
        self
    }

    /// Identify the client to provider usage and diagnostics surfaces.
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.inner = self.inner.user_agent(user_agent);
        self
    }

    pub fn temperature(mut self, temperature: f64) -> Self {
        self.inner = self.inner.temperature(temperature);
        self
    }

    pub fn max_tokens(mut self, max_tokens: u64) -> Self {
        self.inner = self.inner.max_tokens(max_tokens);
        self
    }

    /// Enable Anthropic-compatible ephemeral prompt caching through
    /// OpenRouter. The request field is intentionally absent by default.
    pub fn prompt_cache(mut self, enabled: bool) -> Self {
        self.inner = self.inner.prompt_cache(enabled);
        self
    }

    /// Pin every step in a run to the provider route holding its warm cache.
    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.inner = self.inner.session_id(session_id);
        self
    }

    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.inner = self.inner.nested_reasoning_effort(effort);
        self
    }

    pub fn parallel_tool_calls(mut self, enabled: bool) -> Self {
        self.inner = self.inner.parallel_tool_calls(enabled);
        self
    }

    /// Fetch this endpoint's model catalog.
    pub async fn models(&self) -> Result<Vec<crate::catalog::ModelInfo>, ModelError> {
        list_models(&self.base_url, self.api_key.as_deref()).await
    }
}

#[async_trait]
impl Model for OpenRouterModel {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.inner.generate(context, tools).await
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        self.inner.generate_streaming(context, tools, sink).await
    }
}

/// List the catalog of any OpenAI-compatible endpoint (`GET {base}/models`) in
/// provider order. OpenRouter's catalog is public; the key is optional.
pub async fn list_models(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<crate::catalog::ModelInfo>, ModelError> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = crate::http::client();
    let mut request = client.get(&url).timeout(crate::http::CATALOG_TIMEOUT);
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }
    let response = request.send().await.map_err(|e| {
        // reqwest's Display hides the cause chain; surface it.
        let mut message = e.to_string();
        let mut source = std::error::Error::source(&e);
        while let Some(cause) = source {
            message.push_str(&format!(": {cause}"));
            source = cause.source();
        }
        ModelError::Request(message)
    })?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| crate::http_error::transport_error(&e))?;
    if !status.is_success() {
        return Err(ModelError::Request(format!("HTTP {status}: {body}")));
    }

    #[derive(Deserialize)]
    struct Listing {
        data: Vec<crate::catalog::ModelInfo>,
    }
    let listing: Listing = serde_json::from_str(&body)
        .map_err(|e| ModelError::InvalidResponse(format!("{e}: {body}")))?;
    let models = listing
        .data
        .into_iter()
        .map(normalize_reasoning_efforts)
        .collect::<Vec<_>>();
    Ok(models)
}

fn normalize_reasoning_efforts(mut model: ModelInfo) -> ModelInfo {
    if let Some(reasoning) = &mut model.reasoning {
        if reasoning.supported_efforts == Some(SupportedEfforts::Any) {
            reasoning.supported_efforts = Some(SupportedEfforts::Listed(
                ALL_REASONING_EFFORTS
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            ));
        }
    }
    model
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info(value: serde_json::Value) -> ModelInfo {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn summary_formats_openrouter_metadata() {
        let m = info(json!({
            "id": "openai/gpt-4o", "name": "GPT-4o", "context_length": 128000,
            "pricing": {"prompt": "0.0000025", "completion": "0.00001"}
        }));
        assert_eq!(
            m.summary(),
            "openai/gpt-4o  128k ctx  $2.50/M in $10.00/M out"
        );
    }

    #[test]
    fn summary_survives_bare_openai_entries() {
        let m = info(json!({"id": "gpt-4o", "object": "model", "created": 1234}));
        assert_eq!(m.summary(), "gpt-4o");
    }

    #[test]
    fn free_models_say_free() {
        let m = info(json!({
            "id": "meta-llama/llama-3-8b:free", "context_length": 8192,
            "pricing": {"prompt": "0", "completion": "0"}
        }));
        assert_eq!(m.summary(), "meta-llama/llama-3-8b:free  8k ctx  free");
    }

    #[test]
    fn million_token_contexts_use_m() {
        let m = info(json!({"id": "google/gemini-pro", "context_length": 1000000}));
        assert_eq!(m.summary(), "google/gemini-pro  1M ctx");
    }

    #[test]
    fn catalog_retains_provider_reasoning_capabilities() {
        let m = info(json!({
            "id": "openai/gpt-5",
            "reasoning": {
                "supported_efforts": ["high", "medium", "low"],
                "default_effort": "medium",
                "mandatory": true
            }
        }));
        let reasoning = m.reasoning.unwrap();
        assert_eq!(
            reasoning.supported_efforts.unwrap(),
            SupportedEfforts::Listed(vec!["high".into(), "medium".into(), "low".into()])
        );
        assert_eq!(reasoning.default_effort.as_deref(), Some("medium"));
    }

    #[test]
    fn null_supported_efforts_expands_to_openrouter_gateway_values() {
        let m = normalize_reasoning_efforts(info(json!({
            "id": "openai/future-model",
            "reasoning": {
                "supported_efforts": null,
                "default_effort": "medium"
            }
        })));
        assert_eq!(
            m.reasoning.unwrap().supported_efforts.unwrap(),
            SupportedEfforts::Listed(
                ALL_REASONING_EFFORTS
                    .into_iter()
                    .map(str::to_string)
                    .collect()
            )
        );
    }

    #[test]
    fn omitted_supported_efforts_does_not_invent_a_picker() {
        let m = normalize_reasoning_efforts(info(json!({
            "id": "openai/fixed-model",
            "reasoning": {"default_effort": "medium"}
        })));
        assert!(m.reasoning.unwrap().supported_efforts.is_none());
    }
}
