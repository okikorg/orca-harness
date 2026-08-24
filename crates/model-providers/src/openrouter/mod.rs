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
use orca_harness_core::{Context, DeltaSink, Model, ModelError, ModelResponse, ToolSchema};

pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

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
        self.inner = self.inner.header("X-Title", title);
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

/// List the catalog of any OpenAI-compatible endpoint (`GET {base}/models`),
/// sorted by id. OpenRouter's catalog is public; the key is optional.
pub async fn list_models(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<crate::catalog::ModelInfo>, ModelError> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| ModelError::Request(e.to_string()))?;
    let mut request = client.get(&url);
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
        .map_err(|e| ModelError::Request(e.to_string()))?;
    if !status.is_success() {
        return Err(ModelError::Request(format!("HTTP {status}: {body}")));
    }

    #[derive(Deserialize)]
    struct Listing {
        data: Vec<crate::catalog::ModelInfo>,
    }
    let listing: Listing = serde_json::from_str(&body)
        .map_err(|e| ModelError::InvalidResponse(format!("{e}: {body}")))?;
    let mut models = listing.data;
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
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
}
