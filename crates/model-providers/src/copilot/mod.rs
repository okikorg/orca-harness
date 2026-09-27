//! GitHub Copilot credential exchange and protocol delegation.
//!
//! Credentials are supplied explicitly; device login and persistence belong to
//! the host. Calls on one instance are serialized to retain adapter replay state.
use async_trait::async_trait;
use orca_harness_core::{Context, DeltaSink, Model, ModelError, ModelResponse, ToolSchema};
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::catalog::{ModelInfo, ReasoningCapabilities, SupportedEfforts};
use crate::registry::ProviderPreset;
use crate::{anthropic::AnthropicModel, openai::OpenAiModel, responses::ResponsesModel};

const TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const HEADERS: &[(&str, &str)] = &[
    ("user-agent", "GitHubCopilotChat/0.35.0"),
    ("editor-version", "vscode/1.107.0"),
    ("editor-plugin-version", "copilot-chat/0.35.0"),
    ("copilot-integration-id", "vscode-chat"),
];

pub struct CopilotModel {
    model: String,
    max_tokens: Option<u64>,
    reasoning_effort: Option<String>,
    github_token: Option<String>,
    base_url: Option<String>,
    state: Mutex<State>,
    #[cfg(test)]
    token_url: Option<String>,
    #[cfg(test)]
    api_fixture: Option<String>,
}

struct State {
    adapter: Option<Adapter>,
    expires_at: u64,
    credential: Option<(String, String)>,
    catalog: Option<Vec<CatalogModel>>,
}

enum Adapter {
    Anthropic(AnthropicModel),
    Responses(ResponsesModel),
    Chat(OpenAiModel),
}

// Builder calls consume but do not recreate the adapter (and its replay caches).
macro_rules! map_adapter {
    ($adapter:expr, $method:ident, $value:expr) => {
        match $adapter {
            Adapter::Anthropic(m) => Adapter::Anthropic(m.$method($value)),
            Adapter::Responses(m) => Adapter::Responses(m.$method($value)),
            Adapter::Chat(m) => Adapter::Chat(m.$method($value)),
        }
    };
}

impl Adapter {
    fn model(&self) -> &dyn Model {
        match self {
            Self::Anthropic(m) => m,
            Self::Responses(m) => m,
            Self::Chat(m) => m,
        }
    }

    fn credential(self, token: &str, base: &str) -> Self {
        match self {
            Self::Anthropic(m) => Self::Anthropic(
                m.bearer_token(token)
                    .base_url(format!("{}/v1", base.trim_end_matches('/'))),
            ),
            Self::Responses(m) => Self::Responses(m.api_key(token).base_url(base)),
            Self::Chat(m) => Self::Chat(m.api_key(token).base_url(base)),
        }
    }
}

impl CopilotModel {
    pub fn new(model: impl Into<String>) -> Self {
        let model = model.into();
        Self {
            model,
            max_tokens: None,
            reasoning_effort: None,
            github_token: None,
            base_url: None,
            state: Mutex::new(State {
                adapter: None,
                credential: None,
                catalog: None,
                expires_at: 0,
            }),
            #[cfg(test)]
            token_url: None,
            #[cfg(test)]
            api_fixture: None,
        }
    }

    /// Override the API root (without `/v1`). Must still be HTTPS on a
    /// subdomain of githubcopilot.com; validation happens before exchange.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self.state.get_mut().expires_at = 0;
        self.state.get_mut().catalog = None;
        self
    }

    /// The GitHub token, not the short-lived Copilot access token.
    pub fn api_key(mut self, token: impl Into<String>) -> Self {
        self.github_token = Some(token.into());
        self.state.get_mut().expires_at = 0;
        self.state.get_mut().catalog = None;
        self
    }

    pub fn max_tokens(mut self, tokens: u64) -> Self {
        self.max_tokens = Some(tokens);
        let state = self.state.get_mut();
        state.adapter = state
            .adapter
            .take()
            .map(|a| map_adapter!(a, max_tokens, tokens));
        self
    }

    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        let effort = effort.into();
        self.reasoning_effort = Some(effort.clone());
        let state = self.state.get_mut();
        state.adapter = state
            .adapter
            .take()
            .map(|a| map_adapter!(a, reasoning_effort, effort));
        self
    }

    async fn refresh(&self, state: &mut State) -> Result<(), ModelError> {
        if state.expires_at > now().saturating_add(60) {
            return Ok(());
        }
        if let Some(base) = &self.base_url {
            validate_endpoint(base)?;
        }
        let token = self
            .github_token
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ModelError::Request("Copilot requires a GitHub token".into()))?;
        let url = TOKEN_URL;
        #[cfg(test)]
        let url = self.token_url.as_deref().unwrap_or(url);
        // The shared client never follows redirects, so the GitHub token stays on this origin.
        let mut request = crate::http::client()
            .get(url)
            .bearer_auth(token)
            .header("accept", "application/json")
            .timeout(crate::http::CATALOG_TIMEOUT);
        for &(name, value) in HEADERS {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|_| ModelError::Request("Copilot token exchange transport failed".into()))?;
        if !response.status().is_success() {
            return Err(ModelError::Request(format!(
                "Copilot token exchange failed (HTTP {})",
                response.status()
            )));
        }
        // Do not include the response body or token in diagnostics.
        let credential: Credential = response
            .json()
            .await
            .map_err(|_| ModelError::InvalidResponse("Invalid Copilot token response".into()))?;
        if credential.token.trim().is_empty()
            || reqwest::header::HeaderValue::from_str(&format!("Bearer {}", credential.token))
                .is_err()
            || credential.expires_at <= now()
        {
            return Err(ModelError::InvalidResponse(
                "Invalid or expired Copilot credential".into(),
            ));
        }
        let base = credential
            .endpoints
            .map(|e| e.api)
            .unwrap_or_else(|| ProviderPreset::GithubCopilot.base_url().into());
        validate_endpoint(&base)?;
        let base = self
            .base_url
            .as_deref()
            .unwrap_or(&base)
            .trim_end_matches('/');
        #[cfg(test)]
        let base = self.api_fixture.as_deref().unwrap_or(base);
        // All fallible/async work precedes taking the adapter. Cancellation or
        // failed refresh leaves both replay state and expiration untouched.
        state.adapter = state
            .adapter
            .take()
            .map(|a| a.credential(&credential.token, base));
        state.credential = Some((credential.token, base.to_owned()));
        state.expires_at = credential.expires_at;
        Ok(())
    }

    /// Discover the authenticated account's advertised models. Unknown metadata
    /// remains absent; no capabilities are inferred from model names.
    pub async fn models(&self) -> Result<Vec<ModelInfo>, ModelError> {
        let mut state = self.state.lock().await;
        self.refresh(&mut state).await?;
        self.discover(&mut state).await?;
        Ok(state
            .catalog
            .as_ref()
            .expect("catalog loaded")
            .iter()
            .map(CatalogModel::info)
            .collect())
    }

    async fn discover(&self, state: &mut State) -> Result<(), ModelError> {
        if state.catalog.is_some() {
            return Ok(());
        }
        let (token, base) = state.credential.as_ref().expect("credential loaded");
        let mut request = crate::discovery::get(base, "/models", Some(token))
            .header("accept", "application/json");
        for &(name, value) in HEADERS {
            request = request.header(name, value);
        }
        let invalid = || ModelError::InvalidResponse("Invalid Copilot model catalog".into());
        let value = crate::discovery::fetch_json_redacted(request)
            .await
            .map_err(|e| match e {
                ModelError::InvalidResponse(_) => invalid(),
                e => e,
            })?;
        let catalog: Catalog = serde_json::from_value(value).map_err(|_| invalid())?;
        state.catalog = Some(catalog.data);
        Ok(())
    }

    async fn prepare(&self, state: &mut State) -> Result<(), ModelError> {
        self.refresh(state).await?;
        if state.adapter.is_some() {
            return Ok(());
        }
        self.discover(state).await?;
        let entry = state.catalog.as_ref().expect("catalog loaded").iter()
            .find(|entry| entry.id == self.model)
            .ok_or_else(|| ModelError::Request(
                "Requested model is not in the authenticated Copilot catalog; call models() and choose an advertised ID".into()))?;
        let endpoints = entry.supported_endpoints.as_deref().unwrap_or_default();
        // Prefer Messages, then Responses, then Chat, but only if advertised.
        let mut adapter = if endpoints.iter().any(|e| e == "/v1/messages") {
            Adapter::Anthropic(AnthropicModel::new(&self.model))
        } else if endpoints.iter().any(|e| e == "/responses") {
            Adapter::Responses(ResponsesModel::new(&self.model))
        } else if endpoints.iter().any(|e| e == "/chat/completions") {
            Adapter::Chat(OpenAiModel::new(&self.model))
        } else {
            return Err(ModelError::Request(
                "Copilot model has no supported HTTP endpoint capabilities; choose a model advertising /v1/messages, /responses, or /chat/completions".into()));
        };
        for &(name, value) in HEADERS {
            adapter = match adapter {
                Adapter::Anthropic(m) => Adapter::Anthropic(m.header(name, value)),
                Adapter::Responses(m) => Adapter::Responses(m.header(name, value)),
                Adapter::Chat(m) => Adapter::Chat(m.header(name, value)),
            };
        }
        if let Some(tokens) = self.max_tokens {
            adapter = map_adapter!(adapter, max_tokens, tokens);
        }
        if let Some(effort) = &self.reasoning_effort {
            adapter = map_adapter!(adapter, reasoning_effort, effort.clone());
        }
        let (token, base) = state.credential.as_ref().expect("credential loaded");
        state.adapter = Some(adapter.credential(token, base));
        Ok(())
    }
}

// Wire schema: microsoft/vscode-copilot-chat,
// src/platform/endpoint/common/endpointProvider.ts (IModelAPIResponse).
#[derive(Deserialize)]
struct Catalog {
    data: Vec<CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    id: String,
    name: Option<String>,
    supported_endpoints: Option<Vec<String>>,
    capabilities: Option<Capabilities>,
}

#[derive(Deserialize)]
struct Capabilities {
    limits: Option<Limits>,
    supports: Option<Supports>,
}

#[derive(Deserialize)]
struct Limits {
    max_context_window_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct Supports {
    reasoning_effort: Option<Vec<String>>,
}

impl CatalogModel {
    fn info(&self) -> ModelInfo {
        let capabilities = self.capabilities.as_ref();
        ModelInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            context_length: capabilities
                .and_then(|c| c.limits.as_ref())
                .and_then(|l| l.max_context_window_tokens),
            pricing: None,
            reasoning: capabilities
                .and_then(|c| c.supports.as_ref())
                .and_then(|s| s.reasoning_effort.as_ref())
                .map(|efforts| ReasoningCapabilities {
                    supported_efforts: Some(SupportedEfforts::Listed(efforts.clone())),
                    default_effort: None,
                }),
        }
    }
}

#[derive(Deserialize)]
struct Credential {
    token: String,
    expires_at: u64,
    endpoints: Option<Endpoints>,
}
#[derive(Deserialize)]
struct Endpoints {
    api: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn validate_endpoint(endpoint: &str) -> Result<(), ModelError> {
    let valid = reqwest::Url::parse(endpoint).is_ok_and(|url| {
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| host.ends_with(".githubcopilot.com"))
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && url.query().is_none()
            && url.fragment().is_none()
    });
    if valid {
        Ok(())
    } else {
        Err(ModelError::InvalidResponse(
            "Untrusted Copilot API endpoint".into(),
        ))
    }
}

#[async_trait]
impl Model for CopilotModel {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        let mut state = self.state.lock().await;
        self.prepare(&mut state).await?;
        state
            .adapter
            .as_ref()
            .expect("adapter retained")
            .model()
            .generate(context, tools)
            .await
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        let mut state = self.state.lock().await;
        self.prepare(&mut state).await?;
        state
            .adapter
            .as_ref()
            .expect("adapter retained")
            .model()
            .generate_streaming(context, tools, sink)
            .await
    }
}

#[cfg(test)]
mod tests;
