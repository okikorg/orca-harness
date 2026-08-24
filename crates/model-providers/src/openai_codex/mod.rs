//! OpenAI Codex Responses adapter backed by a ChatGPT subscription.
//! This is deliberately separate from OpenAI API-key billing.

mod request;
mod stream;

pub use orca_harness_provider_auth::{
    BearerCredential, CredentialError, CredentialErrorKind, CredentialSource, StaticCredential,
};

use crate::catalog::ModelInfo;
use async_trait::async_trait;
use futures_util::StreamExt;
use orca_harness_core::{Context, DeltaSink, Model, ModelError, ModelResponse, ToolSchema};
use std::{collections::HashMap, sync::Arc};
use stream::{Accumulator, SseBuffer};
use tokio::sync::Mutex;

pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
// Codex uses this protocol-client version for catalog compatibility filtering.
// It is deliberately independent of Orcacode's package version.
const CODEX_PROTOCOL_VERSION: &str = "0.144.1";

fn catalog_url(base_url: &str) -> String {
    format!(
        "{}/models?client_version={CODEX_PROTOCOL_VERSION}",
        base_url.trim_end_matches('/')
    )
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct CodexModelInfo {
    #[serde(alias = "slug")]
    pub id: String,
    #[serde(default, alias = "display_name")]
    pub name: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
}

/// Fetch the subscription-backed Codex catalog with OAuth and account headers.
pub async fn list_models(
    credentials: Arc<dyn CodexCredentialSource>,
) -> Result<Vec<ModelInfo>, ModelError> {
    let credential = credentials.codex_credential().await.map_err(auth_error)?;
    let rejected = credential.bearer.access_token.clone();
    let client = reqwest::Client::new();
    let mut response = send_catalog_request(&client, credential).await?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        let renewed = credentials
            .refresh_codex(&rejected)
            .await
            .map_err(auth_error)?;
        response = send_catalog_request(&client, renewed).await?;
    }
    parse_catalog_response(response).await
}

async fn send_catalog_request(
    client: &reqwest::Client,
    credential: CodexCredential,
) -> Result<reqwest::Response, ModelError> {
    client
        .get(catalog_url(CODEX_BASE_URL))
        .bearer_auth(credential.bearer.access_token)
        .header("chatgpt-account-id", credential.account_id)
        .header("OpenAI-Beta", "responses=experimental")
        .header("originator", "orcacode")
        .send()
        .await
        .map_err(|error| ModelError::Request(error.to_string()))
}

async fn parse_catalog_response(response: reqwest::Response) -> Result<Vec<ModelInfo>, ModelError> {
    let status = response.status();
    if !status.is_success() {
        return Err(if status == reqwest::StatusCode::UNAUTHORIZED {
            ModelError::Authentication("Codex model catalog rejected the login".into())
        } else {
            ModelError::Request(format!("Codex model catalog returned HTTP {status}"))
        });
    }
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|error| ModelError::InvalidResponse(error.to_string()))?;
    let rows = value
        .get("models")
        .or_else(|| value.get("data"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ModelError::InvalidResponse("Codex catalog has no models array".into()))?;
    rows.iter()
        .cloned()
        .map(|row| {
            let info: CodexModelInfo = serde_json::from_value(row)
                .map_err(|error| ModelError::InvalidResponse(error.to_string()))?;
            Ok(ModelInfo {
                id: info.id,
                name: info.name,
                context_length: info.context_window,
                pricing: None,
            })
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct CodexCredential {
    pub bearer: BearerCredential,
    pub account_id: String,
}

#[async_trait]
pub trait CodexCredentialSource: CredentialSource {
    async fn codex_credential(&self) -> Result<CodexCredential, CredentialError>;
    async fn refresh_codex(
        &self,
        rejected_access_token: &str,
    ) -> Result<CodexCredential, CredentialError>;
}

pub struct OpenAiCodexModel {
    client: reqwest::Client,
    base_url: String,
    model: String,
    credentials: Arc<dyn CodexCredentialSource>,
    reasoning_by_call: Mutex<HashMap<String, Vec<serde_json::Value>>>,
}

impl OpenAiCodexModel {
    pub fn new(model: impl Into<String>, credentials: Arc<dyn CodexCredentialSource>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: CODEX_BASE_URL.into(),
            model: model.into(),
            credentials,
            reasoning_by_call: Mutex::new(HashMap::new()),
        }
    }

    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    async fn send(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        stream: bool,
    ) -> Result<reqwest::Response, ModelError> {
        let credential = self
            .credentials
            .codex_credential()
            .await
            .map_err(auth_error)?;
        let rejected_token = credential.bearer.access_token.clone();
        let response = self.send_with(context, tools, stream, credential).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let renewed = self
                .credentials
                .refresh_codex(&rejected_token)
                .await
                .map_err(auth_error)?;
            return self
                .validate(self.send_with(context, tools, stream, renewed).await?)
                .await;
        }
        self.validate(response).await
    }

    async fn send_with(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        stream: bool,
        credential: CodexCredential,
    ) -> Result<reqwest::Response, ModelError> {
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let continuation = self.continuation_for(context).await;
        let mut request = self
            .client
            .post(url)
            .bearer_auth(credential.bearer.access_token)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "orcacode")
            .header(
                "accept",
                if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .json(&request::body(
                &self.model,
                context,
                tools,
                stream,
                &continuation,
            ));
        request = request.header("chatgpt-account-id", credential.account_id);
        request
            .send()
            .await
            .map_err(|e| ModelError::Request(e.to_string()))
    }

    async fn validate(&self, response: reqwest::Response) -> Result<reqwest::Response, ModelError> {
        if !response.status().is_success() {
            let status = response.status();
            let bytes = response.bytes().await.unwrap_or_default();
            let body = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)])
                .chars()
                .map(|ch| {
                    if ch.is_control() && !ch.is_whitespace() {
                        ' '
                    } else {
                        ch
                    }
                })
                .collect::<String>();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err(ModelError::Authentication(format!(
                    "Codex login was rejected after refresh: {body}"
                )));
            }
            return Err(ModelError::Request(format!("Codex HTTP {status}: {body}")));
        }
        Ok(response)
    }
}

fn auth_error(error: CredentialError) -> ModelError {
    ModelError::Authentication(error.message)
}

#[async_trait]
impl Model for OpenAiCodexModel {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        let response = self.send(context, tools, true).await?;
        let collected = collect_stream(response, None).await?;
        self.remember_reasoning(context, &collected).await;
        Ok(collected.response)
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        let response = self.send(context, tools, true).await?;
        let collected = collect_stream(response, Some(sink)).await?;
        self.remember_reasoning(context, &collected).await;
        Ok(collected.response)
    }
}

impl OpenAiCodexModel {
    async fn continuation_for(&self, context: &Context) -> Vec<serde_json::Value> {
        let pending = self.reasoning_by_call.lock().await;
        context
            .messages()
            .iter()
            .rev()
            .find_map(|message| match message {
                orca_harness_core::Message::Tool { results } => results
                    .iter()
                    .find_map(|result| pending.get(&result.call_id).cloned()),
                _ => None,
            })
            .unwrap_or_default()
    }

    async fn remember_reasoning(&self, context: &Context, collected: &Collected) {
        let mut pending = self.reasoning_by_call.lock().await;
        for message in context.messages() {
            if let orca_harness_core::Message::Tool { results } = message {
                for result in results {
                    pending.remove(&result.call_id);
                }
            }
        }
        if let ModelResponse::ToolCalls { calls, .. } = &collected.response {
            for call in calls {
                pending.insert(call.id.clone(), collected.reasoning.clone());
            }
        }
    }
}

struct Collected {
    response: ModelResponse,
    reasoning: Vec<serde_json::Value>,
}

async fn collect_stream(
    response: reqwest::Response,
    sink: Option<&dyn DeltaSink>,
) -> Result<Collected, ModelError> {
    let mut bytes = response.bytes_stream();
    let mut frames = SseBuffer::default();
    let mut accumulator = Accumulator::default();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk.map_err(|e| ModelError::Request(e.to_string()))?;
        for payload in frames.push(&chunk)? {
            if payload == "[DONE]" {
                continue;
            }
            for delta in accumulator.apply(&payload)? {
                if let Some(sink) = sink {
                    sink.emit(delta).await;
                }
            }
        }
    }
    let reasoning = accumulator.reasoning().to_vec();
    let response = accumulator.finish()?;
    Ok(Collected {
        response,
        reasoning,
    })
}

#[cfg(test)]
mod catalog_tests {
    #[test]
    fn catalog_identifies_the_client_version() {
        assert_eq!(
            super::catalog_url("https://example.test/codex/"),
            "https://example.test/codex/models?client_version=0.144.1"
        );
    }
}
