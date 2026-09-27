//! Registry-backed model construction. The core continues to see only `Model`.
//! Provider differences come from the preset's [`Spec`] row, never its ID.
use std::sync::Arc;

use async_trait::async_trait;
use orca_harness_core::{Context, DeltaSink, Model, ModelError, ModelResponse, ToolSchema};
use tokio::sync::OnceCell;

use crate::registry::{Adapter, Credential, Discovery, KeyPlacement, Protocol, ProviderPreset};

/// Host identification forwarded to gateways that attribute traffic.
#[derive(Debug, Clone, Default)]
pub struct Attribution {
    pub referer: Option<String>,
    pub title: Option<String>,
    pub categories: Option<String>,
}

/// A named provider using the same model contract as the individual adapters.
/// Credentials are explicit; environment lookup and persistence remain host concerns.
/// The selected adapter is retained across turns to preserve opaque reasoning state.
pub struct ProviderModel {
    preset: ProviderPreset,
    model: String,
    protocol: Option<Protocol>,
    base_url: Option<String>,
    api_key: Option<String>,
    max_tokens: Option<u64>,
    reasoning_effort: Option<String>,
    user_agent: Option<String>,
    prompt_cache: bool,
    session_id: Option<String>,
    attribution: Attribution,
    codex_credentials: Option<Arc<dyn crate::openai_codex::CodexCredentialSource>>,
    inner: OnceCell<Arc<dyn Model>>,
}

impl ProviderModel {
    pub fn new(preset: ProviderPreset, model: impl Into<String>) -> Self {
        Self {
            preset,
            model: model.into(),
            protocol: None,
            base_url: None,
            api_key: None,
            max_tokens: None,
            reasoning_effort: None,
            user_agent: None,
            prompt_cache: false,
            session_id: None,
            attribution: Attribution::default(),
            codex_credentials: None,
            inner: OnceCell::new(),
        }
    }

    fn reset(mut self) -> Self {
        self.inner = OnceCell::new();
        self
    }

    /// Explicit API root override, preserved even when equal to the preset default.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self.reset()
    }
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self.reset()
    }
    pub fn max_tokens(mut self, count: u64) -> Self {
        self.max_tokens = Some(count);
        self.reset()
    }
    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self.reset()
    }
    /// Sent where the adapter supports custom headers.
    pub fn user_agent(mut self, agent: impl Into<String>) -> Self {
        self.user_agent = Some(agent.into());
        self.reset()
    }
    /// Request provider-side prompt caching where the protocol offers it.
    pub fn prompt_cache(mut self, enabled: bool) -> Self {
        self.prompt_cache = enabled;
        self.reset()
    }
    /// A stable per-session ID used as the cache key or session hint.
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self.reset()
    }
    pub fn attribution(mut self, attribution: Attribution) -> Self {
        self.attribution = attribution;
        self.reset()
    }

    /// Subscription OAuth remains behind the existing credential boundary.
    pub fn codex_credentials(
        mut self,
        source: Arc<dyn crate::openai_codex::CodexCredentialSource>,
    ) -> Self {
        self.codex_credentials = Some(source);
        self.reset()
    }

    /// Choose a transport explicitly for a mixed-protocol service.
    /// Also set `base_url` to that interface's complete API root.
    pub fn protocol(mut self, protocol: Protocol) -> Self {
        self.protocol = Some(protocol);
        self.reset()
    }

    /// Discover models visible to the current credential, without a fallback catalog.
    pub async fn models(&self) -> Result<Vec<crate::ModelInfo>, ModelError> {
        tokio::time::timeout(crate::http::CATALOG_TIMEOUT, self.discover())
            .await
            .map_err(|_| ModelError::Request("model discovery timed out".into()))?
    }

    async fn discover(&self) -> Result<Vec<crate::ModelInfo>, ModelError> {
        let discovery = self.preset.spec().discovery;
        if discovery == Discovery::Unsupported {
            return Err(ModelError::Request(format!(
                "{}: model discovery unavailable; supply a model ID manually and configure its transport",
                self.preset.id()
            )));
        }
        let key = self.credential(true)?;
        let root = self.endpoint()?.1;
        match discovery {
            Discovery::Unsupported => unreachable!("handled above"),
            Discovery::OpenAiModels | Discovery::Ollama => {
                crate::discovery::list_openai_models(&root, key).await
            }
            Discovery::OpenRouter => crate::openrouter::list_models(&root, key).await,
            Discovery::Vercel => crate::vercel::list_models(&root, key).await,
            Discovery::CheaperInference => crate::cheaperinference::list_models(&root).await,
            Discovery::Radius => crate::pi_messages::list_models(&root, key).await,
            Discovery::Anthropic => crate::anthropic::list_models(&root, key).await,
            Discovery::Google => {
                let mut model = crate::GoogleModel::new("").base_url(root);
                if let Some(key) = key {
                    model = model.api_key(key);
                }
                model.models().await
            }
            Discovery::Copilot => self.copilot("").models().await,
            Discovery::Cursor => crate::cursor::list_models(&root, required(key)?).await,
            Discovery::Codex => crate::openai_codex::list_models(&root, self.codex()?).await,
        }
    }

    /// The active model's context window, best effort.
    pub async fn context_window(&self) -> Option<u64> {
        let key = self.credential(true).ok()?;
        match self.preset.spec().discovery {
            // `/models` lists dated IDs, so resolve an alias directly.
            Discovery::Anthropic => {
                let root = self.endpoint().ok()?.1;
                crate::anthropic::retrieve_model(&root, key, &self.model)
                    .await
                    .ok()?
                    .context_length
            }
            Discovery::Ollama => {
                let root = self.endpoint().ok()?.1;
                crate::discovery::ollama_context_window(&root, &self.model).await
            }
            _ => {
                self.models()
                    .await
                    .ok()?
                    .into_iter()
                    .find(|model| model.id == self.model)?
                    .context_length
            }
        }
    }

    /// The API key, required unless the preset needs none or discovery is public.
    fn credential(&self, discovery: bool) -> Result<Option<&str>, ModelError> {
        let key = self.api_key.as_deref().filter(|key| !key.trim().is_empty());
        let spec = self.preset.spec();
        match spec.credential {
            Credential::ApiKey { env, .. }
                if key.is_none() && !(discovery && spec.public_catalog) =>
            {
                Err(ModelError::Authentication(format!(
                    "{} requires a credential ({env})",
                    spec.id
                )))
            }
            _ => Ok(key),
        }
    }

    fn codex(&self) -> Result<Arc<dyn crate::openai_codex::CodexCredentialSource>, ModelError> {
        self.codex_credentials.clone().ok_or_else(|| {
            ModelError::Authentication(format!(
                "{} requires codex_credentials with a CodexCredentialSource",
                self.preset.id()
            ))
        })
    }

    fn copilot(&self, model: &str) -> crate::CopilotModel {
        let mut copilot = crate::CopilotModel::new(model);
        if let Some(key) = &self.api_key {
            copilot = copilot.api_key(key);
        }
        // Copilot's API root comes from its token exchange unless overridden.
        if let Some(url) = &self.base_url {
            copilot = copilot.base_url(url);
        }
        copilot
    }

    async fn adapter(&self) -> Result<&Arc<dyn Model>, ModelError> {
        self.inner.get_or_try_init(|| async { self.build() }).await
    }

    /// The transport and resolved API root.
    fn endpoint(&self) -> Result<(Protocol, String), ModelError> {
        let default = self.preset.spec().protocol;
        let protocol = match self.protocol {
            // The preset's URL serves only its own dialect.
            Some(explicit) if Some(explicit) != default && self.base_url.is_none() => {
                return Err(ModelError::Request(format!(
                    "{}: protocol {} requires an explicit base_url for that interface",
                    self.preset.id(),
                    explicit.name()
                )))
            }
            Some(explicit) => explicit,
            None => self.preset.protocol()?,
        };
        let base_url = self.preset.resolve_base_url(self.base_url.as_deref())?;
        Ok((protocol, base_url))
    }

    fn build(&self) -> Result<Arc<dyn Model>, ModelError> {
        let spec = self.preset.spec();
        let quirks = spec.quirks;
        let (protocol, base_url) = self.endpoint()?;
        let key = self.credential(false)?;
        let placement = match spec.credential {
            Credential::ApiKey { placement, .. } => placement,
            Credential::OAuth | Credential::None => KeyPlacement::Native,
        };
        let user_agent = quirks.user_agent.or(self.user_agent.as_deref());

        // Output and effort controls every adapter below shares.
        macro_rules! tuned {
            ($model:expr) => {{
                let mut model = $model;
                if let Some(count) = self.max_tokens {
                    model = model.max_tokens(count);
                }
                if let Some(effort) = &self.reasoning_effort {
                    model = model.reasoning_effort(effort);
                }
                model
            }};
        }
        // Key placement for Bearer-native adapters that accept custom headers.
        macro_rules! keyed {
            ($model:expr) => {{
                let mut model = $model;
                if let Some(key) = key {
                    model = match placement {
                        KeyPlacement::Native | KeyPlacement::Bearer => model.api_key(key),
                        KeyPlacement::Header(name) => model.header(name, key),
                        KeyPlacement::CloudflareGateway => {
                            model.header("cf-aig-authorization", format!("Bearer {key}"))
                        }
                    };
                }
                if let Some(agent) = user_agent {
                    model = model.header(reqwest::header::USER_AGENT.as_str(), agent);
                }
                model
            }};
        }
        // Adapters with only their native key scheme.
        macro_rules! native {
            ($model:expr) => {{
                let mut model = $model;
                if let Some(key) = key {
                    model = model.api_key(key);
                }
                model
            }};
        }

        // Wrapped adapters serve only the preset's own dialect; an explicit
        // foreign protocol goes straight to that protocol's adapter.
        let native_route = self.protocol.is_none_or(|p| Some(p) == spec.protocol);
        match spec.adapter {
            _ if !native_route => {}
            Adapter::Copilot => return Ok(Arc::new(tuned!(self.copilot(&self.model)))),
            Adapter::OpenRouter => {
                let mut model = tuned!(native!(
                    crate::OpenRouterModel::new(&self.model).base_url(base_url)
                ));
                if let Some(agent) = user_agent {
                    model = model.user_agent(agent);
                }
                let Attribution {
                    referer,
                    title,
                    categories,
                } = &self.attribution;
                if let Some(referer) = referer {
                    model = model.referer(referer);
                }
                if let Some(title) = title {
                    model = model.title(title);
                }
                if let Some(categories) = categories {
                    model = model.categories(categories);
                }
                if self.prompt_cache {
                    model = model.prompt_cache(true);
                    if let Some(session) = &self.session_id {
                        model = model.session_id(session);
                    }
                }
                return Ok(Arc::new(model));
            }
            Adapter::Protocol => {}
        }

        if self.max_tokens.is_some() && !protocol.supports_max_tokens() {
            return Err(ModelError::Request(format!(
                "{} ({}) does not support max_tokens",
                spec.id,
                protocol.name()
            )));
        }
        Ok(match protocol {
            Protocol::ChatCompletions => {
                let mut model = keyed!(crate::OpenAiModel::new(&self.model)
                    .base_url(base_url)
                    .replay_reasoning_content(quirks.replay_reasoning_content));
                if let Some(count) = self.max_tokens {
                    model = model.max_tokens(count);
                }
                if let Some(effort) = &self.reasoning_effort {
                    model = if quirks.nested_reasoning_effort {
                        model.nested_reasoning_effort(effort)
                    } else {
                        model.reasoning_effort(effort)
                    };
                }
                Arc::new(model)
            }
            Protocol::Anthropic => {
                let mut model = crate::AnthropicModel::new(&self.model)
                    .base_url(base_url)
                    .prompt_cache(self.prompt_cache);
                if let Some(key) = key {
                    model = match placement {
                        KeyPlacement::Native => model.api_key(key),
                        KeyPlacement::Bearer => model.bearer_token(key),
                        KeyPlacement::Header(name) => model.header(name, key),
                        KeyPlacement::CloudflareGateway => model.gateway_token(key),
                    };
                }
                if let Some(agent) = user_agent {
                    model = model.header(reqwest::header::USER_AGENT.as_str(), agent);
                }
                Arc::new(tuned!(model))
            }
            Protocol::Responses => {
                Arc::new(tuned!(keyed!(crate::ResponsesModel::new(&self.model)
                    .base_url(base_url)
                    .encrypted_reasoning(quirks.encrypted_reasoning))))
            }
            Protocol::Google | Protocol::Vertex => Arc::new(tuned!(native!(
                crate::GoogleModel::new(&self.model).base_url(base_url)
            ))),
            Protocol::Bedrock => Arc::new(tuned!(native!(
                crate::BedrockModel::new(&self.model).base_url(base_url)
            ))),
            Protocol::PiMessages => {
                Arc::new(tuned!(native!(crate::PiMessagesModel::new(&self.model)
                    .provider(spec.id)
                    .base_url(base_url))))
            }
            Protocol::Cursor => {
                if self.reasoning_effort.is_some() {
                    return Err(ModelError::Request(
                        "Cursor does not expose reasoning-effort controls".into(),
                    ));
                }
                Arc::new(native!(
                    crate::CursorModel::new(&self.model).base_url(base_url)
                ))
            }
            Protocol::Codex => {
                let mut model =
                    crate::OpenAiCodexModel::new(&self.model, self.codex()?).base_url(base_url);
                if let Some(effort) = &self.reasoning_effort {
                    model = model.reasoning_effort(effort);
                }
                if let (true, Some(session)) = (self.prompt_cache, &self.session_id) {
                    model = model.prompt_cache_key(session);
                }
                Arc::new(model)
            }
        })
    }
}

fn required(key: Option<&str>) -> Result<&str, ModelError> {
    key.ok_or_else(|| ModelError::Authentication("discovery requires a credential".into()))
}

#[async_trait]
impl Model for ProviderModel {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.adapter().await?.generate(context, tools).await
    }
    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        self.adapter()
            .await?
            .generate_streaming(context, tools, sink)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoints_are_provider_only() {
        for (id, model, expected) in [
            (
                "minimax",
                "MiniMax-M2.7",
                "https://api.minimax.io/anthropic/v1",
            ),
            ("mistral", "custom-model", "https://api.mistral.ai/v1"),
            (
                "openrouter",
                "openrouter/auto",
                "https://openrouter.ai/api/v1",
            ),
        ] {
            let provider = ProviderModel::new(ProviderPreset::from_id(id).unwrap(), model);
            assert_eq!(provider.endpoint().unwrap().1, expected);
        }
    }
    #[test]
    fn explicit_override_is_not_modified() {
        let model = ProviderModel::new(ProviderPreset::Minimax, "test")
            .base_url("http://localhost:8080/custom/v1");
        assert_eq!(
            model.endpoint().unwrap().1,
            "http://localhost:8080/custom/v1"
        );
    }
    #[test]
    fn foreign_protocol_needs_its_own_root() {
        let model = ProviderModel::new(ProviderPreset::Deepseek, "m").protocol(Protocol::Anthropic);
        assert!(model.endpoint().is_err());
        let model = model.base_url("http://localhost:8080/anthropic");
        assert_eq!(model.endpoint().unwrap().0, Protocol::Anthropic);
        let same =
            ProviderModel::new(ProviderPreset::Deepseek, "m").protocol(Protocol::ChatCompletions);
        assert!(same.endpoint().is_ok());
    }
    #[tokio::test]
    async fn adapter_is_reused_for_multi_turn_state() {
        let model = ProviderModel::new(ProviderPreset::Deepseek, "deepseek-chat").api_key("test");
        let first = model.adapter().await.unwrap();
        let second = model.adapter().await.unwrap();
        assert!(Arc::ptr_eq(first, second));
    }
}
