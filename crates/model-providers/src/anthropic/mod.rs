//! Native Anthropic Messages API adapter (API-key authentication).
//! Prompt caching is opt-in for SDK consumers. No `thinking` key is sent, so
//! each model applies its own default; signed thinking blocks returned by a
//! thinking-on model are replayed on the turn that produced them. Provider-
//! hosted tools are not enabled by this adapter.

mod catalog;
mod request;
mod stream;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use orca_harness_core::{
    Context, DeltaSink, Message, Model, ModelError, ModelResponse, ToolSchema,
};
use serde_json::Value;
use tokio::sync::Mutex;

pub use catalog::{list_models, retrieve_model};
pub use request::input_schema;

pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
const API_VERSION: &str = "2023-06-01";

/// A native Messages API model. Supply credentials explicitly with `api_key`;
/// environment lookup and credential persistence belong to the host.
pub struct AnthropicModel {
    model: String,
    base_url: String,
    api_key: Option<String>,
    max_tokens: u64,
    temperature: Option<f64>,
    prompt_cache: bool,
    reasoning_effort: Option<String>,
    /// Signed thinking blocks keyed by the tool-call ids of the turn that
    /// produced them. A thinking-on model requires them back on that turn,
    /// and `Context` is provider-neutral, so they are held here instead.
    thinking_by_call: Mutex<HashMap<String, Arc<[Value]>>>,
}

impl AnthropicModel {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: ANTHROPIC_BASE_URL.into(),
            api_key: None,
            max_tokens: 8192,
            temperature: None,
            prompt_cache: false,
            reasoning_effort: None,
            thinking_by_call: Mutex::new(HashMap::new()),
        }
    }

    /// API root including `/v1`.
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Required Messages API output cap; defaults to 8192.
    pub fn max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Enable automatic ephemeral prompt caching (provider-default TTL).
    pub fn prompt_cache(mut self, enabled: bool) -> Self {
        self.prompt_cache = enabled;
        self
    }

    /// Set `output_config.effort` on models that support it.
    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    pub async fn models(&self) -> Result<Vec<crate::ModelInfo>, ModelError> {
        list_models(&self.base_url, self.api_key.as_deref()).await
    }

    async fn prepare_request(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<reqwest::RequestBuilder, ModelError> {
        let body = {
            let thinking = self.thinking_by_call.lock().await;
            self.request_body(context, tools, &thinking)?
        };
        Ok(authenticated_request(
            &format!("{}/messages", self.base_url.trim_end_matches('/')),
            self.api_key.as_deref(),
            reqwest::Method::POST,
        )?
        .header("accept", "text/event-stream")
        .json(&body))
    }

    async fn generate_with(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: Option<&dyn DeltaSink>,
    ) -> Result<ModelResponse, ModelError> {
        // Always stream, including when the host only wants the final result.
        let response = self
            .prepare_request(context, tools)
            .await?
            .send()
            .await
            .map_err(|error| crate::http_error::transport_error(&error))?;
        let response = crate::http_error::check_response(response).await?;
        let mut bytes = response.bytes_stream();
        let mut frames = crate::sse::SseBuffer::default();
        let mut accumulator = stream::Accumulator::default();
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(|error| crate::http_error::transport_error(&error))?;
            for payload in frames.push(&chunk)? {
                for delta in accumulator.apply(&payload)? {
                    if let Some(sink) = sink {
                        sink.emit(delta).await;
                    }
                }
                if accumulator.stopped() {
                    return self.remember(context, accumulator.finish()?).await;
                }
            }
        }
        let collected = accumulator.finish()?;
        self.remember(context, collected).await
    }

    /// Retain this turn's thinking blocks under the tool calls they belong to,
    /// and drop entries whose results have already come back.
    async fn remember(
        &self,
        context: &Context,
        collected: stream::Collected,
    ) -> Result<ModelResponse, ModelError> {
        let mut pending = self.thinking_by_call.lock().await;
        // Sweep on every turn, not just tool turns, so a session that ends in
        // a plain answer still drains what its tool results retired.
        for message in context.messages() {
            if let Message::Tool { results } = message {
                for result in results {
                    pending.remove(&result.call_id);
                }
            }
        }
        if let ModelResponse::ToolCalls { calls, .. } = &collected.response {
            if !collected.thinking.is_empty() {
                for call in calls {
                    pending.insert(call.id.clone(), collected.thinking.clone());
                }
            }
        }
        drop(pending);
        Ok(collected.response)
    }
}

fn authenticated_request(
    url: &str,
    api_key: Option<&str>,
    method: reqwest::Method,
) -> Result<reqwest::RequestBuilder, ModelError> {
    let key = api_key
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| ModelError::Authentication("Anthropic requires an API key".into()))?;
    let mut key = reqwest::header::HeaderValue::from_str(key)
        .map_err(|_| ModelError::Authentication("invalid Anthropic API key header".into()))?;
    key.set_sensitive(true);
    Ok(crate::http::client()
        .request(method, url)
        .header("x-api-key", key)
        .header("anthropic-version", API_VERSION))
}

#[async_trait]
impl Model for AnthropicModel {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.generate_with(context, tools, None).await
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        self.generate_with(context, tools, Some(sink)).await
    }
}
