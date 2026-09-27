//! Registry-backed model construction. The core continues to see only `Model`.
use std::sync::Arc;

use async_trait::async_trait;
use orca_harness_core::{Context, DeltaSink, Model, ModelError, ModelResponse, ToolSchema};
use tokio::sync::OnceCell;

use crate::registry::{Protocol, ProviderPreset};

/// A named provider using the same model contract as the individual adapters.
/// Credentials are explicit; environment lookup and persistence remain host concerns.
/// The selected adapter is retained across turns to preserve opaque reasoning state.
pub struct ProviderModel {
    preset: ProviderPreset,
    model: String,
    base_url: Option<String>,
    api_key: Option<String>,
    max_tokens: Option<u64>,
    reasoning_effort: Option<String>,
    codex_credentials: Option<Arc<dyn crate::openai_codex::CodexCredentialSource>>,
    inner: OnceCell<Arc<dyn Model>>,
}

impl ProviderModel {
    pub fn new(preset: ProviderPreset, model: impl Into<String>) -> Self {
        Self {
            preset,
            model: model.into(),
            base_url: None,
            api_key: None,
            max_tokens: None,
            reasoning_effort: None,
            codex_credentials: None,
            inner: OnceCell::new(),
        }
    }

    /// Explicit API root override, preserved even when equal to the preset default.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self.inner = OnceCell::new();
        self
    }
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self.inner = OnceCell::new();
        self
    }
    pub fn max_tokens(mut self, count: u64) -> Self {
        self.max_tokens = Some(count);
        self.inner = OnceCell::new();
        self
    }
    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self.inner = OnceCell::new();
        self
    }

    /// Subscription OAuth remains behind the existing credential boundary.
    pub fn codex_credentials(
        mut self,
        source: Arc<dyn crate::openai_codex::CodexCredentialSource>,
    ) -> Self {
        self.codex_credentials = Some(source);
        self.inner = OnceCell::new();
        self
    }

    /// Built-in provider catalog. Unlike a network catalog this also works for
    /// services without a model discovery endpoint; account availability may differ.
    pub async fn models(&self) -> Result<Vec<crate::ModelInfo>, ModelError> {
        self.endpoint()?;
        Ok(self.preset.models())
    }

    async fn adapter(&self) -> Result<&Arc<dyn Model>, ModelError> {
        self.inner.get_or_try_init(|| async { self.build() }).await
    }

    fn endpoint(&self) -> Result<(Protocol, String), ModelError> {
        // Preserve OpenRouter's normalized Chat Completions interface for
        // every model, including catalog rows offering an alternate dialect.
        if self.preset.id() == "openrouter" {
            return Ok((
                Protocol::ChatCompletions,
                self.preset.resolve_base_url(self.base_url.as_deref())?,
            ));
        }
        let route = self.preset.route(&self.model);
        let mut base_url = route.resolve_base_url(self.preset, self.base_url.as_deref())?;
        // Catalog roots follow upstream conventions; adapters take the API root.
        if self.base_url.is_none() {
            match route.protocol {
                Protocol::Anthropic if !base_url.ends_with("/v1") => base_url.push_str("/v1"),
                Protocol::ChatCompletions
                    if self.preset.id() == "mistral" && !base_url.ends_with("/v1") =>
                {
                    base_url.push_str("/v1")
                }
                Protocol::Vertex => {
                    let project = std::env::var("GOOGLE_CLOUD_PROJECT").ok().filter(|s| !s.trim().is_empty())
                        .ok_or_else(|| ModelError::Request("google-vertex requires GOOGLE_CLOUD_PROJECT or an explicit base URL".into()))?;
                    let location = std::env::var("GOOGLE_CLOUD_LOCATION")
                        .unwrap_or_else(|_| "us-central1".into());
                    crate::registry::validate_segment(
                        "google-vertex",
                        "GOOGLE_CLOUD_PROJECT",
                        &project,
                    )?;
                    crate::registry::validate_segment(
                        "google-vertex",
                        "GOOGLE_CLOUD_LOCATION",
                        &location,
                    )?;
                    base_url = format!(
                        "{}/v1/projects/{project}/locations/{location}/publishers/google",
                        base_url.trim_end_matches('/')
                    );
                }
                Protocol::Responses
                    if self.preset.id() == "azure-openai-responses"
                        && !base_url.contains("/openai/") =>
                {
                    base_url = format!("{}/openai/v1", base_url.trim_end_matches('/'));
                }
                _ => {}
            }
        }
        Ok((route.protocol, base_url))
    }

    fn build(&self) -> Result<Arc<dyn Model>, ModelError> {
        let (protocol, base_url) = self.endpoint()?;
        let id = self.preset.id();
        let key = self.api_key.as_deref().filter(|key| !key.trim().is_empty());
        if self.preset.key_env().is_some() && key.is_none() {
            return Err(ModelError::Authentication(format!(
                "{id} requires a credential ({})",
                self.preset.key_env().unwrap()
            )));
        }
        // All adapters below expose the same optional output/effort controls.
        macro_rules! configured {
            ($model:expr) => {{
                let mut model = $model;
                if let Some(key) = key.filter(|_| id != "cloudflare-ai-gateway") {
                    model = model.api_key(key);
                }
                if let Some(count) = self.max_tokens {
                    model = model.max_tokens(count);
                }
                if let Some(effort) = &self.reasoning_effort {
                    model = model.reasoning_effort(effort);
                }
                Arc::new(model) as Arc<dyn Model>
            }};
        }
        if id == "github-copilot" {
            let mut model = crate::copilot::CopilotModel::new(&self.model);
            if let Some(url) = &self.base_url {
                model = model.base_url(url);
            }
            return Ok(configured!(model));
        }
        if id == "openrouter" {
            return Ok(configured!(
                crate::OpenRouterModel::new(&self.model).base_url(base_url)
            ));
        }
        Ok(match protocol {
            Protocol::ChatCompletions => {
                let mut model = crate::OpenAiModel::new(&self.model)
                    .base_url(base_url)
                    .replay_reasoning_content(matches!(
                        id,
                        "deepseek" | "moonshotai" | "moonshotai-cn" | "zai" | "zai-coding-cn"
                    ));
                if id == "vercel" {
                    if let Some(effort) = &self.reasoning_effort {
                        model = model.nested_reasoning_effort(effort);
                    }
                    if let Some(key) = key {
                        model = model.api_key(key);
                    }
                    if let Some(count) = self.max_tokens {
                        model = model.max_tokens(count);
                    }
                    return Ok(Arc::new(model));
                }
                configured!(model)
            }
            Protocol::Anthropic => {
                let mut model = crate::AnthropicModel::new(&self.model).base_url(base_url);
                if matches!(id, "databricks-unity-gateway" | "snowflake-cortex") {
                    if let Some(key) = key {
                        model = model.bearer_token(key);
                    }
                }
                if id == "cloudflare-ai-gateway" {
                    model = model.gateway_token(key.expect("credential validated above"));
                }
                if id == "kimi-coding" {
                    model = model.header("user-agent", "orcacode");
                }
                configured!(model)
            }
            Protocol::Responses => {
                let mut model = crate::ResponsesModel::new(&self.model)
                    .base_url(base_url)
                    .encrypted_reasoning(!matches!(id, "xai" | "meta"));
                if id == "azure-openai-responses" {
                    if let Some(key) = key {
                        model = model.header("api-key", key);
                    }
                }
                configured!(model)
            }
            Protocol::Google => {
                configured!(crate::google::GoogleModel::new(&self.model).base_url(base_url))
            }
            Protocol::Vertex => {
                configured!(crate::google::GoogleModel::new(&self.model).base_url(base_url))
            }
            Protocol::Bedrock => {
                configured!(crate::bedrock::BedrockModel::new(&self.model).base_url(base_url))
            }
            Protocol::PiMessages => {
                configured!(crate::pi_messages::PiMessagesModel::new(&self.model).base_url(base_url))
            }
            Protocol::Cursor => {
                if self.max_tokens.is_some() || self.reasoning_effort.is_some() {
                    return Err(ModelError::Request(
                        "Cursor does not expose output-token or reasoning-effort controls".into(),
                    ));
                }
                let mut model = crate::cursor::CursorModel::new(&self.model).base_url(base_url);
                if let Some(key) = key {
                    model = model.api_key(key);
                }
                Arc::new(model)
            }
            Protocol::Codex => {
                let credentials = self.codex_credentials.clone().ok_or_else(|| {
                    ModelError::Authentication(
                        "openai-codex requires codex_credentials with a CodexCredentialSource"
                            .into(),
                    )
                })?;
                if self.max_tokens.is_some() {
                    return Err(ModelError::Request(
                        "openai-codex does not support max_tokens".into(),
                    ));
                }
                let base_url = if self.base_url.is_none() {
                    crate::openai_codex::CODEX_BASE_URL.to_owned()
                } else {
                    base_url
                };
                let mut model =
                    crate::OpenAiCodexModel::new(&self.model, credentials).base_url(base_url);
                if let Some(effort) = &self.reasoning_effort {
                    model = model.reasoning_effort(effort);
                }
                Arc::new(model)
            }
        })
    }
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
    fn endpoints_follow_model_protocol_not_provider_default() {
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
    fn explicit_preset_default_survives_mixed_model_routing() {
        let preset = ProviderPreset::Minimax;
        let automatic = ProviderModel::new(preset, "MiniMax-M2.7");
        assert_ne!(automatic.endpoint().unwrap().1, preset.base_url());
        let explicit = automatic.base_url(preset.base_url());
        assert_eq!(explicit.endpoint().unwrap().1, preset.base_url());
    }

    #[tokio::test]
    async fn adapter_is_reused_for_multi_turn_state() {
        let model = ProviderModel::new(ProviderPreset::Deepseek, "deepseek-chat").api_key("test");
        let first = model.adapter().await.unwrap();
        let second = model.adapter().await.unwrap();
        assert!(Arc::ptr_eq(first, second));
    }
}
