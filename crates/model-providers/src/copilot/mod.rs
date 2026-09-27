//! GitHub Copilot credential exchange and protocol delegation.
//!
//! Credentials are supplied explicitly; device login and persistence belong to
//! the host. Calls on one instance are serialized to retain adapter replay state.
use async_trait::async_trait;
use orca_harness_core::{Context, DeltaSink, Model, ModelError, ModelResponse, ToolSchema};
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::registry::{Protocol, ProviderPreset};
use crate::{anthropic::AnthropicModel, openai::OpenAiModel, responses::ResponsesModel};

const TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const HEADERS: &[(&str, &str)] = &[
    ("user-agent", "GitHubCopilotChat/0.35.0"),
    ("editor-version", "vscode/1.107.0"),
    ("editor-plugin-version", "copilot-chat/0.35.0"),
    ("copilot-integration-id", "vscode-chat"),
];

pub struct CopilotModel {
    github_token: Option<String>,
    base_url: Option<String>,
    state: Mutex<State>,
    #[cfg(test)]
    token_url: Option<String>,
}

struct State {
    adapter: Option<Adapter>,
    expires_at: u64,
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
        let mut adapter = match ProviderPreset::GithubCopilot.route(&model).protocol {
            Protocol::Anthropic => Adapter::Anthropic(AnthropicModel::new(model)),
            Protocol::Responses => Adapter::Responses(ResponsesModel::new(model)),
            Protocol::ChatCompletions => Adapter::Chat(OpenAiModel::new(model)),
            _ => unreachable!("Copilot registry uses Messages, Responses or Chat Completions"),
        };
        for &(name, value) in HEADERS {
            adapter = match adapter {
                Adapter::Anthropic(m) => Adapter::Anthropic(m.header(name, value)),
                Adapter::Responses(m) => Adapter::Responses(m.header(name, value)),
                Adapter::Chat(m) => Adapter::Chat(m.header(name, value)),
            };
        }
        Self {
            github_token: None,
            base_url: None,
            state: Mutex::new(State {
                adapter: Some(adapter),
                expires_at: 0,
            }),
            #[cfg(test)]
            token_url: None,
        }
    }

    /// Override the API root (without `/v1`). Must still be HTTPS on a
    /// subdomain of githubcopilot.com; validation happens before exchange.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self.state.get_mut().expires_at = 0;
        self
    }

    /// The GitHub token, not the short-lived Copilot access token.
    pub fn api_key(mut self, token: impl Into<String>) -> Self {
        self.github_token = Some(token.into());
        self.state.get_mut().expires_at = 0;
        self
    }

    pub fn max_tokens(mut self, tokens: u64) -> Self {
        let state = self.state.get_mut();
        state.adapter = state
            .adapter
            .take()
            .map(|a| map_adapter!(a, max_tokens, tokens));
        self
    }

    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        let effort = effort.into();
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
        // Never use the shared redirect-following client for GitHub credentials.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ModelError::Request("Cannot build Copilot token client".into()))?;
        let mut request = client
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
        // All fallible/async work precedes taking the adapter. Cancellation or
        // failed refresh leaves both replay state and expiration untouched.
        state.adapter = state
            .adapter
            .take()
            .map(|a| a.credential(&credential.token, base));
        state.expires_at = credential.expires_at;
        Ok(())
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
        self.refresh(&mut state).await?;
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
        self.refresh(&mut state).await?;
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
