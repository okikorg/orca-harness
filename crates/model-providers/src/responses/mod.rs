//! API-key-backed OpenAI Responses protocol adapter (including Azure endpoints).

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use orca_harness_core::{Context, DeltaSink, Model, ModelError, ModelResponse, ToolSchema};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::openai_codex::{collect_stream, request, stream::Accumulator};

/// Responses API model using API-key authentication, not Codex subscription credentials.
pub struct ResponsesModel {
    model: String,
    base_url: String,
    api_key: Option<String>,
    headers: Vec<(String, String)>,
    max_tokens: Option<u64>,
    reasoning_effort: Option<String>,
    encrypted_reasoning: bool,
    reasoning_by_call: Mutex<HashMap<String, Arc<[Value]>>>,
}

impl ResponsesModel {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: None,
            headers: Vec::new(),
            max_tokens: None,
            reasoning_effort: None,
            encrypted_reasoning: true,
            reasoning_by_call: Mutex::new(HashMap::new()),
        }
    }

    /// Root URL before `/responses`, e.g. an Azure deployment URL with `api-version`.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn max_tokens(mut self, tokens: u64) -> Self {
        self.max_tokens = Some(tokens);
        self
    }

    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    /// OpenAI supports encrypted reasoning replay; other Responses services
    /// (such as xAI) need not support the OpenAI-specific include selector.
    pub fn encrypted_reasoning(mut self, enabled: bool) -> Self {
        self.encrypted_reasoning = enabled;
        self
    }

    fn url(&self) -> String {
        // Append to the URL path, not to the query (Azure uses ?api-version=...).
        if let Some((path, query)) = self.base_url.split_once('?') {
            format!("{}/responses?{query}", path.trim_end_matches('/'))
        } else {
            format!("{}/responses", self.base_url.trim_end_matches('/'))
        }
    }

    async fn send(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<reqwest::Response, ModelError> {
        let request = {
            let pending = self.reasoning_by_call.lock().await;
            // Unlike Codex, the entire context is sent, so every earlier tool
            // turn replays its own reasoning.
            let mut body = request::body(
                &self.model,
                context,
                tools,
                true,
                request::Options {
                    reasoning_effort: self.reasoning_effort.as_deref(),
                    prompt_cache_key: None,
                    always_reason: false,
                    encrypted_reasoning: self.encrypted_reasoning,
                },
                |call_id| pending.get(call_id).map(|reasoning| &reasoning[..]),
            );
            if let Some(tokens) = self.max_tokens {
                body["max_output_tokens"] = tokens.into();
            }
            let request = crate::http::client()
                .post(self.url())
                .header("accept", "text/event-stream")
                .json(&body);
            crate::openai::authorize(request, self.api_key.as_deref(), &self.headers)
        };
        crate::sse::send(request).await
    }

    async fn collect(
        &self,
        response: reqwest::Response,
        sink: Option<&dyn DeltaSink>,
    ) -> Result<ModelResponse, ModelError> {
        let collected = collect_stream(
            response,
            sink,
            Accumulator::skipping_unencrypted_reasoning(),
        )
        .await?;
        if let ModelResponse::ToolCalls { calls, .. } = &collected.response {
            if self.encrypted_reasoning && !collected.reasoning.is_empty() {
                let mut pending = self.reasoning_by_call.lock().await;
                for call in calls {
                    pending.insert(call.id.clone(), collected.reasoning.clone());
                }
            }
        }
        Ok(collected.response)
    }
}

#[async_trait]
impl Model for ResponsesModel {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.collect(self.send(context, tools).await?, None).await
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        self.collect(self.send(context, tools).await?, Some(sink))
            .await
    }
}

#[cfg(test)]
mod tests;
