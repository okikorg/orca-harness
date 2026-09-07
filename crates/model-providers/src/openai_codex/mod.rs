//! OpenAI Codex Responses adapter backed by a ChatGPT subscription.
//! This is deliberately separate from OpenAI API-key billing.

mod request;
mod stream;

pub use orca_harness_provider_auth::{
    BearerCredential, CredentialError, CredentialErrorKind, CredentialSource, StaticCredential,
};

use crate::catalog::{ModelInfo, ReasoningCapabilities, SupportedEfforts};
use async_trait::async_trait;
use futures_util::StreamExt;
use orca_harness_core::{Context, DeltaSink, Model, ModelError, ModelResponse, ToolSchema};
use std::{collections::HashMap, sync::Arc};
use stream::{Accumulator, SseBuffer};
use tokio::sync::Mutex;

pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const ORCACODE_USER_AGENT: &str = concat!("orcacode/", env!("CARGO_PKG_VERSION"));
// Codex uses this protocol-client version for catalog compatibility filtering.
// It is deliberately independent of Orcacode's package version.
const CODEX_PROTOCOL_VERSION: &str = "0.144.1";

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(ORCACODE_USER_AGENT)
        .build()
        .expect("the static Orcacode user agent is valid")
}

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
    #[serde(default)]
    pub supported_reasoning_levels: Vec<CodexReasoningLevel>,
    #[serde(default)]
    pub default_reasoning_level: Option<String>,
    /// Lower values are preferred by the provider.
    #[serde(default)]
    pub priority: i32,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct CodexReasoningLevel {
    pub effort: String,
}

impl From<CodexModelInfo> for ModelInfo {
    fn from(info: CodexModelInfo) -> Self {
        Self {
            id: info.id,
            name: info.name,
            context_length: info.context_window,
            pricing: None,
            reasoning: (!info.supported_reasoning_levels.is_empty()).then(|| {
                ReasoningCapabilities {
                    supported_efforts: Some(SupportedEfforts::Listed(
                        info.supported_reasoning_levels
                            .into_iter()
                            .map(|level| level.effort)
                            .collect(),
                    )),
                    default_effort: info.default_reasoning_level,
                }
            }),
        }
    }
}

/// Fetch the subscription-backed Codex catalog with OAuth and account headers.
pub async fn list_models(
    credentials: Arc<dyn CodexCredentialSource>,
) -> Result<Vec<ModelInfo>, ModelError> {
    let credential = credentials.codex_credential().await.map_err(auth_error)?;
    let rejected = credential.bearer.access_token.clone();
    let client = client();
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
        .map_err(|error| crate::http_error::transport_error(&error))
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
    let mut models = rows
        .iter()
        .cloned()
        .map(|row| {
            let info: CodexModelInfo = serde_json::from_value(row)
                .map_err(|error| ModelError::InvalidResponse(error.to_string()))?;
            Ok(info)
        })
        .collect::<Result<Vec<CodexModelInfo>, ModelError>>()?;
    models.sort_by_key(|model| model.priority);
    Ok(models.into_iter().map(Into::into).collect())
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
    reasoning_effort: Option<String>,
    prompt_cache_key: Option<String>,
    credentials: Arc<dyn CodexCredentialSource>,
    reasoning_by_call: Mutex<HashMap<String, Vec<serde_json::Value>>>,
}

impl OpenAiCodexModel {
    pub fn new(model: impl Into<String>, credentials: Arc<dyn CodexCredentialSource>) -> Self {
        Self {
            client: client(),
            base_url: CODEX_BASE_URL.into(),
            model: model.into(),
            reasoning_effort: None,
            prompt_cache_key: None,
            credentials,
            reasoning_by_call: Mutex::new(HashMap::new()),
        }
    }

    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    /// Keep subscription-backed Responses requests on one prompt-cache key.
    pub fn prompt_cache_key(mut self, prompt_cache_key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(prompt_cache_key.into());
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
            return crate::http_error::check_response(
                self.send_with(context, tools, stream, renewed).await?,
            )
            .await;
        }
        crate::http_error::check_response(response).await
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
                self.reasoning_effort.as_deref(),
                self.prompt_cache_key.as_deref(),
            ));
        request = request.header("chatgpt-account-id", credential.account_id);
        request
            .send()
            .await
            .map_err(|e| crate::http_error::transport_error(&e))
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
        let chunk = chunk.map_err(|e| crate::http_error::transport_error(&e))?;
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
    use serde_json::json;

    #[test]
    fn catalog_identifies_the_client_version() {
        assert_eq!(
            super::catalog_url("https://example.test/codex/"),
            "https://example.test/codex/models?client_version=0.144.1"
        );
    }

    #[test]
    fn catalog_maps_supported_and_default_reasoning_levels() {
        let wire: super::CodexModelInfo = serde_json::from_value(json!({
            "slug": "gpt-test",
            "display_name": "GPT Test",
            "context_window": 200000,
            "supported_reasoning_levels": [
                {"effort": "low", "description": "Fast"},
                {"effort": "high", "description": "Deep"}
            ],
            "default_reasoning_level": "high",
            "priority": 2
        }))
        .unwrap();

        let model: crate::ModelInfo = wire.into();
        let reasoning = model.reasoning.unwrap();
        assert_eq!(
            reasoning.supported_efforts.unwrap(),
            crate::SupportedEfforts::Listed(vec!["low".into(), "high".into()])
        );
        assert_eq!(reasoning.default_effort.as_deref(), Some("high"));
    }

    #[test]
    fn catalog_priority_is_deserialized() {
        let model: super::CodexModelInfo = serde_json::from_value(json!({
            "slug": "preferred",
            "priority": 0
        }))
        .unwrap();
        assert_eq!(model.priority, 0);
    }
}
