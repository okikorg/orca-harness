//! Native Gemini GenerateContent API adapter. Credentials and Vertex endpoint
//! selection are supplied by the host; no environment lookup is performed.
mod request;
mod stream;
#[cfg(test)]
mod tests;

use async_trait::async_trait;
use futures_util::StreamExt;
use orca_harness_core::{Context, DeltaSink, Model, ModelError, ModelResponse, ToolSchema};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};
use tokio::sync::Mutex;

pub const GOOGLE_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct GoogleModel {
    model: String,
    base_url: String,
    api_key: Option<String>,
    bearer_token: Option<String>,
    max_tokens: Option<u64>,
    reasoning_effort: Option<String>,
    thinking_budget: Option<i32>,
    thinking_level: Option<String>,
    // Provider-only parts cannot be persisted in Context. Associate them with
    // the calls from the turn that produced them, never with the latest turn.
    signatures: Mutex<HashMap<String, Arc<[Value]>>>,
}

impl GoogleModel {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: GOOGLE_BASE_URL.into(),
            api_key: None,
            bearer_token: None,
            max_tokens: None,
            reasoning_effort: None,
            thinking_budget: None,
            thinking_level: None,
            signatures: Mutex::new(HashMap::new()),
        }
    }
    /// API root including `/v1beta`; for Vertex supply a root ending in
    /// `/projects/{project}/locations/{location}/publishers/google`.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self.bearer_token = None;
        self
    }
    pub fn bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self.api_key = None;
        self
    }
    pub fn max_tokens(mut self, count: u64) -> Self {
        self.max_tokens = Some(count);
        self
    }
    /// Compatibility builder: generic effort is unsupported because Gemini discovery
    /// advertises no effort-to-native-setting mapping. Generation returns an error.
    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    /// Send a native token budget (-1 requests automatic budgeting).
    /// Model-specific support and limits are validated by the provider.
    pub fn thinking_budget(mut self, budget: i32) -> Self {
        self.thinking_budget = Some(budget);
        self
    }

    /// Send a native thinking level verbatim; support is validated by the provider.
    pub fn thinking_level(mut self, level: impl Into<String>) -> Self {
        self.thinking_level = Some(level.into());
        self
    }

    fn request(
        &self,
        url: String,
        method: reqwest::Method,
    ) -> Result<reqwest::RequestBuilder, ModelError> {
        // Never forward credentials through redirects, including same-origin redirects.
        static CLIENT: OnceLock<Result<reqwest::Client, reqwest::Error>> = OnceLock::new();
        let client = CLIENT
            .get_or_init(|| {
                reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
            })
            .as_ref()
            .map_err(|e| crate::http_error::transport_error(e))?;
        let mut request = client.request(method, url);
        if let Some(token) = &self.bearer_token {
            let token = token.trim();
            if token.is_empty() {
                return Err(ModelError::Authentication(
                    "Google requires a bearer token".into(),
                ));
            }
            request = request.bearer_auth(token);
        } else {
            let key = self
                .api_key
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    ModelError::Authentication("Google requires an API key or bearer token".into())
                })?;
            let mut header = reqwest::header::HeaderValue::from_str(key)
                .map_err(|_| ModelError::Authentication("invalid Google API key header".into()))?;
            header.set_sensitive(true);
            request = request.header("x-goog-api-key", header);
        }
        Ok(request)
    }

    fn url(&self, suffix: &str) -> Result<String, ModelError> {
        let model = self.model.strip_prefix("models/").unwrap_or(&self.model);
        if model.is_empty()
            || !model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
        {
            return Err(ModelError::Request("invalid Google model id".into()));
        }
        Ok(format!(
            "{}/models/{model}{suffix}",
            self.base_url.trim_end_matches('/')
        ))
    }

    /// List Gemini API models advertising generateContent support.
    /// Vertex publisher catalogs use a different API and are not supported here.
    pub async fn models(&self) -> Result<Vec<crate::ModelInfo>, ModelError> {
        if self.base_url.contains("/publishers/")
            || reqwest::Url::parse(&self.base_url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
                .is_some_and(|host| {
                    host == "aiplatform.googleapis.com"
                        || host.ends_with("-aiplatform.googleapis.com")
                })
        {
            return Err(ModelError::Request(
                "Vertex model discovery unavailable: publisher catalogs are not Gemini ListModels"
                    .into(),
            ));
        }
        let mut result = Vec::new();
        let mut next: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        loop {
            let url = format!("{}/models", self.base_url.trim_end_matches('/'));
            let mut request = self.request(url, reqwest::Method::GET)?;
            if let Some(token) = &next {
                request = request.query(&[("pageToken", token)]);
            }
            let page = parse_models_page(crate::discovery::fetch_json(request).await?)?;
            result.extend(
                page.models
                    .into_iter()
                    .filter(|row| {
                        row.supported_generation_methods
                            .iter()
                            .any(|method| method == "generateContent")
                    })
                    .map(|row| crate::ModelInfo {
                        id: row.name.strip_prefix("models/").unwrap_or(&row.name).into(),
                        name: row.display_name,
                        context_length: row.input_token_limit,
                        pricing: None,
                        // ListModels does not publish a generic reasoning-effort mapping.
                        reasoning: None,
                    }),
            );
            next = page.next_page_token.filter(|token| !token.is_empty());
            match &next {
                Some(token) if !seen.insert(token.clone()) => {
                    return Err(ModelError::InvalidResponse(
                        "Google repeated page token".into(),
                    ))
                }
                None => return Ok(result),
                _ => {}
            }
        }
    }

    async fn generate_with(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: Option<&dyn DeltaSink>,
    ) -> Result<ModelResponse, ModelError> {
        let body = {
            let signatures = self.signatures.lock().await;
            self.body(context, tools, &signatures)?
        };
        let response = self
            .request(
                self.url(":streamGenerateContent?alt=sse")?,
                reqwest::Method::POST,
            )?
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| crate::http_error::transport_error(&e))?;
        let response = crate::http_error::check_response(response).await?;
        let mut bytes = response.bytes_stream();
        let mut frames = crate::sse::SseBuffer::default();
        let mut state = stream::Accumulator::default();
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(|e| crate::http_error::transport_error(&e))?;
            for payload in frames.push(&chunk)? {
                for delta in state.apply(&payload)? {
                    if let Some(sink) = sink {
                        sink.emit(delta).await;
                    }
                }
            }
        }
        let collected = state.finish()?;
        self.update_signatures(&collected).await;
        Ok(collected.response)
    }

    async fn update_signatures(&self, collected: &stream::Collected) {
        let mut signatures = self.signatures.lock().await;
        // Context has no conversation identity: absence here cannot retire another
        // conversation's signatures. Retain replay data for this model's lifetime.
        if let ModelResponse::ToolCalls { calls, .. } = &collected.response {
            let thoughts: Vec<Value> = collected
                .parts
                .iter()
                .filter(|p| p["thought"] == true)
                .cloned()
                .collect();
            for (call, part) in calls.iter().zip(
                collected
                    .parts
                    .iter()
                    .filter(|p| p.get("functionCall").is_some()),
            ) {
                let mut saved = thoughts.clone();
                saved.push(part.clone());
                signatures.insert(call.id.clone(), saved.into());
            }
        }
    }
}

#[async_trait]
impl Model for GoogleModel {
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

// Require the Gemini collection shape, rather than accepting an unrelated API's
// response as an empty catalog. Unknown metadata remains forward compatible.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelsPage {
    models: Vec<CatalogModel>,
    next_page_token: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogModel {
    name: String,
    display_name: Option<String>,
    input_token_limit: Option<u64>,
    #[serde(default)]
    supported_generation_methods: Vec<String>,
}

fn parse_models_page(value: Value) -> Result<ModelsPage, ModelError> {
    let page: ModelsPage = serde_json::from_value(value).map_err(|e| {
        ModelError::InvalidResponse(format!("invalid Gemini ListModels response: {e}"))
    })?;
    if page.models.iter().any(|row| {
        row.name
            .strip_prefix("models/")
            .unwrap_or(&row.name)
            .is_empty()
    }) {
        return Err(ModelError::InvalidResponse(
            "Google model name must not be empty".into(),
        ));
    }
    Ok(page)
}
