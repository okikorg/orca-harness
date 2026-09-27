//! API-key-backed OpenAI Responses protocol adapter (including Azure endpoints).

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use futures_util::StreamExt;
use orca_harness_core::{
    Context, DeltaSink, Message, Model, ModelError, ModelResponse, ToolSchema,
};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{
    openai_codex::{request, stream::Accumulator},
    sse::SseBuffer,
};

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
        let mut body = request::body(
            &self.model,
            context,
            tools,
            true,
            &[],
            self.reasoning_effort.as_deref(),
            None,
        );
        // Codex always reasons. Generic Responses also serves non-reasoning
        // models, which reject a reasoning configuration.
        if self.reasoning_effort.is_none() {
            body.as_object_mut().unwrap().remove("reasoning");
        }
        if !self.encrypted_reasoning {
            body.as_object_mut().unwrap().remove("include");
        } else {
            self.replay_reasoning(context, &mut body).await;
        }
        if let Some(tokens) = self.max_tokens {
            body["max_output_tokens"] = tokens.into();
        }
        let mut request = crate::http::client()
            .post(self.url())
            .header("accept", "text/event-stream")
            .json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request
            .send()
            .await
            .map_err(|e| crate::http_error::transport_error(&e))?;
        crate::http_error::check_response(response).await
    }

    // The Codex request builder inserts only one continuation. Generic Responses
    // sends the entire context, so every earlier tool turn needs its own item.
    async fn replay_reasoning(&self, context: &Context, body: &mut Value) {
        let pending = self.reasoning_by_call.lock().await;
        let input = body["input"].as_array_mut().unwrap();
        let mut position = 0;
        for message in context.messages() {
            match message {
                Message::User { .. } => position += 1,
                Message::Assistant {
                    content,
                    tool_calls,
                } => {
                    if content.is_some() {
                        position += 1;
                    }
                    // All calls from one response share the same reasoning item(s).
                    if let Some(reasoning) =
                        tool_calls.iter().find_map(|call| pending.get(&call.id))
                    {
                        let count = reasoning.len();
                        input.splice(position..position, reasoning.iter().cloned());
                        position += count;
                    }
                    position += tool_calls.len();
                }
                Message::Tool { results } => position += results.len(),
                Message::System { .. } => {}
            }
        }
    }

    async fn collect(
        &self,
        _context: &Context,
        response: reqwest::Response,
        sink: Option<&dyn DeltaSink>,
    ) -> Result<ModelResponse, ModelError> {
        let mut bytes = response.bytes_stream();
        let mut frames = SseBuffer::default();
        let mut accumulator = Accumulator::default();
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(|e| crate::http_error::transport_error(&e))?;
            for payload in frames.push(&chunk).map_err(generic_error)? {
                if payload == "[DONE]" {
                    continue;
                }
                // Some Responses implementations emit reasoning summaries without
                // encrypted content. There is nothing to replay in that case.
                if let Ok(event) = serde_json::from_str::<Value>(&payload) {
                    if event["type"] == "response.output_item.done"
                        && event["item"]["type"] == "reasoning"
                        && event["item"]["encrypted_content"].as_str().is_none()
                    {
                        continue;
                    }
                }
                for delta in accumulator.apply(&payload).map_err(generic_error)? {
                    if let Some(sink) = sink {
                        sink.emit(delta).await;
                    }
                }
            }
        }
        let reasoning: Arc<[Value]> = accumulator.take_reasoning().into();
        let result = accumulator.finish().map_err(generic_error)?;
        let mut pending = self.reasoning_by_call.lock().await;
        if let ModelResponse::ToolCalls { calls, .. } = &result {
            if self.encrypted_reasoning && !reasoning.is_empty() {
                for call in calls {
                    pending.insert(call.id.clone(), reasoning.clone());
                }
            }
        }
        Ok(result)
    }
}

fn generic_error(error: ModelError) -> ModelError {
    match error {
        ModelError::InvalidResponse(message) => {
            ModelError::InvalidResponse(message.replace("Codex", "Responses"))
        }
        other => other,
    }
}

#[async_trait]
impl Model for ResponsesModel {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.collect(context, self.send(context, tools).await?, None)
            .await
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        self.collect(context, self.send(context, tools).await?, Some(sink))
            .await
    }
}

#[cfg(test)]
mod tests;
