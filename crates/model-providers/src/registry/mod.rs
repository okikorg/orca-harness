//! Built-in provider identifiers and factual per-model protocol routes.
//!
//! The embedded catalog is a snapshot, not a live discovery service. Custom
//! model IDs fall back to the provider default route; callers can override URLs.
use crate::ModelInfo;
use orca_harness_core::ModelError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    ChatCompletions,
    Anthropic,
    Responses,
    Codex,
    Google,
    Vertex,
    Bedrock,
    Cursor,
    PiMessages,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub protocol: Protocol,
    pub base_url: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderPreset {
    AmazonBedrock,
    AntLing,
    Anthropic,
    AzureOpenAiResponses,
    Baseten,
    Cerebras,
    CloudflareAiGateway,
    CloudflareWorkersAi,
    Cursor,
    DatabricksUnityGateway,
    Deepseek,
    Fireworks,
    GithubCopilot,
    Google,
    GoogleVertex,
    Groq,
    Huggingface,
    KimiCoding,
    Meta,
    Minimax,
    MinimaxCn,
    Mistral,
    Moonshotai,
    MoonshotaiCn,
    Nvidia,
    OpenAi,
    OpenAiCodex,
    Opencode,
    OpencodeGo,
    Openrouter,
    QwenTokenPlan,
    QwenTokenPlanCn,
    QwenTokenPlanIndividual,
    Radius,
    SnowflakeCortex,
    Together,
    VercelAiGateway,
    Xai,
    Xiaomi,
    XiaomiTokenPlanAms,
    XiaomiTokenPlanCn,
    XiaomiTokenPlanSgp,
    Zai,
    ZaiCodingCn,
    Vercel,
    Cheaperinference,
    Local,
}

pub const ALL: &[ProviderPreset] = &[
    ProviderPreset::AmazonBedrock,
    ProviderPreset::AntLing,
    ProviderPreset::Anthropic,
    ProviderPreset::AzureOpenAiResponses,
    ProviderPreset::Baseten,
    ProviderPreset::Cerebras,
    ProviderPreset::CloudflareAiGateway,
    ProviderPreset::CloudflareWorkersAi,
    ProviderPreset::Cursor,
    ProviderPreset::DatabricksUnityGateway,
    ProviderPreset::Deepseek,
    ProviderPreset::Fireworks,
    ProviderPreset::GithubCopilot,
    ProviderPreset::Google,
    ProviderPreset::GoogleVertex,
    ProviderPreset::Groq,
    ProviderPreset::Huggingface,
    ProviderPreset::KimiCoding,
    ProviderPreset::Meta,
    ProviderPreset::Minimax,
    ProviderPreset::MinimaxCn,
    ProviderPreset::Mistral,
    ProviderPreset::Moonshotai,
    ProviderPreset::MoonshotaiCn,
    ProviderPreset::Nvidia,
    ProviderPreset::OpenAi,
    ProviderPreset::OpenAiCodex,
    ProviderPreset::Opencode,
    ProviderPreset::OpencodeGo,
    ProviderPreset::Openrouter,
    ProviderPreset::QwenTokenPlan,
    ProviderPreset::QwenTokenPlanCn,
    ProviderPreset::QwenTokenPlanIndividual,
    ProviderPreset::Radius,
    ProviderPreset::SnowflakeCortex,
    ProviderPreset::Together,
    ProviderPreset::VercelAiGateway,
    ProviderPreset::Xai,
    ProviderPreset::Xiaomi,
    ProviderPreset::XiaomiTokenPlanAms,
    ProviderPreset::XiaomiTokenPlanCn,
    ProviderPreset::XiaomiTokenPlanSgp,
    ProviderPreset::Zai,
    ProviderPreset::ZaiCodingCn,
    ProviderPreset::Vercel,
    ProviderPreset::Cheaperinference,
    ProviderPreset::Local,
];

impl ProviderPreset {
    pub const ALL: &'static [Self] = ALL;
    pub fn from_id(id: &str) -> Option<Self> {
        ALL.iter().copied().find(|p| p.id() == id)
    }
    pub const fn id(self) -> &'static str {
        match self {
            Self::AmazonBedrock => "amazon-bedrock",
            Self::AntLing => "ant-ling",
            Self::Anthropic => "anthropic",
            Self::AzureOpenAiResponses => "azure-openai-responses",
            Self::Baseten => "baseten",
            Self::Cerebras => "cerebras",
            Self::CloudflareAiGateway => "cloudflare-ai-gateway",
            Self::CloudflareWorkersAi => "cloudflare-workers-ai",
            Self::Cursor => "cursor",
            Self::DatabricksUnityGateway => "databricks-unity-gateway",
            Self::Deepseek => "deepseek",
            Self::Fireworks => "fireworks",
            Self::GithubCopilot => "github-copilot",
            Self::Google => "google",
            Self::GoogleVertex => "google-vertex",
            Self::Groq => "groq",
            Self::Huggingface => "huggingface",
            Self::KimiCoding => "kimi-coding",
            Self::Meta => "meta",
            Self::Minimax => "minimax",
            Self::MinimaxCn => "minimax-cn",
            Self::Mistral => "mistral",
            Self::Moonshotai => "moonshotai",
            Self::MoonshotaiCn => "moonshotai-cn",
            Self::Nvidia => "nvidia",
            Self::OpenAi => "openai",
            Self::OpenAiCodex => "openai-codex",
            Self::Opencode => "opencode",
            Self::OpencodeGo => "opencode-go",
            Self::Openrouter => "openrouter",
            Self::QwenTokenPlan => "qwen-token-plan",
            Self::QwenTokenPlanCn => "qwen-token-plan-cn",
            Self::QwenTokenPlanIndividual => "qwen-token-plan-individual",
            Self::Radius => "radius",
            Self::SnowflakeCortex => "snowflake-cortex",
            Self::Together => "together",
            Self::VercelAiGateway => "vercel-ai-gateway",
            Self::Xai => "xai",
            Self::Xiaomi => "xiaomi",
            Self::XiaomiTokenPlanAms => "xiaomi-token-plan-ams",
            Self::XiaomiTokenPlanCn => "xiaomi-token-plan-cn",
            Self::XiaomiTokenPlanSgp => "xiaomi-token-plan-sgp",
            Self::Zai => "zai",
            Self::ZaiCodingCn => "zai-coding-cn",
            Self::Vercel => "vercel",
            Self::Cheaperinference => "cheaperinference",
            Self::Local => "local",
        }
    }
    pub const fn base_url(self) -> &'static str {
        match self {
            Self::AmazonBedrock => "https://bedrock-runtime.us-east-1.amazonaws.com",
            Self::AntLing => "https://api.ant-ling.com/v1",
            Self::Anthropic => "https://api.anthropic.com",
            Self::AzureOpenAiResponses => "",
            Self::Baseten => "https://inference.baseten.co/v1",
            Self::Cerebras => "https://api.cerebras.ai/v1",
            Self::CloudflareAiGateway => "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/anthropic",
            Self::CloudflareWorkersAi => "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1",
            Self::Cursor => "https://agentn.us.api5.cursor.sh",
            Self::DatabricksUnityGateway => "{DATABRICKS_HOST}/ai-gateway/anthropic",
            Self::Deepseek => "https://api.deepseek.com",
            Self::Fireworks => "https://api.fireworks.ai/inference",
            Self::GithubCopilot => "https://api.individual.githubcopilot.com",
            Self::Google => "https://generativelanguage.googleapis.com/v1beta",
            Self::GoogleVertex => "https://{location}-aiplatform.googleapis.com",
            Self::Groq => "https://api.groq.com/openai/v1",
            Self::Huggingface => "https://router.huggingface.co/v1",
            Self::KimiCoding => "https://api.kimi.com/coding",
            Self::Meta => "https://api.meta.ai/v1",
            Self::Minimax => "https://api.minimax.io/anthropic",
            Self::MinimaxCn => "https://api.minimaxi.com/anthropic",
            Self::Mistral => "https://api.mistral.ai",
            Self::Moonshotai => "https://api.moonshot.ai/v1",
            Self::MoonshotaiCn => "https://api.moonshot.cn/v1",
            Self::Nvidia => "https://integrate.api.nvidia.com/v1",
            Self::OpenAi => "https://api.openai.com/v1",
            Self::OpenAiCodex => "https://chatgpt.com/backend-api",
            Self::Opencode => "https://opencode.ai/zen",
            Self::OpencodeGo => "https://opencode.ai/zen/go",
            Self::Openrouter => "https://openrouter.ai/api/v1",
            Self::QwenTokenPlan => "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
            Self::QwenTokenPlanCn => "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
            Self::QwenTokenPlanIndividual => "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
            Self::Radius => "https://radius.pi.dev/v1",
            Self::SnowflakeCortex => "{SNOWFLAKE_CORTEX_BASE_URL}",
            Self::Together => "https://api.together.ai/v1",
            Self::VercelAiGateway => "https://ai-gateway.vercel.sh",
            Self::Xai => "https://api.x.ai/v1",
            Self::Xiaomi => "https://api.xiaomimimo.com/v1",
            Self::XiaomiTokenPlanAms => "https://token-plan-ams.xiaomimimo.com/v1",
            Self::XiaomiTokenPlanCn => "https://token-plan-cn.xiaomimimo.com/v1",
            Self::XiaomiTokenPlanSgp => "https://token-plan-sgp.xiaomimimo.com/v1",
            Self::Zai => "https://api.z.ai/api/coding/paas/v4",
            Self::ZaiCodingCn => "https://open.bigmodel.cn/api/coding/paas/v4",
            Self::Vercel => "https://ai-gateway.vercel.sh/v1",
            Self::Cheaperinference => "https://api.cheaperinference.com/v1",
            Self::Local => "http://localhost:11434/v1",
        }
    }
    /// Credential lookup is the host's responsibility; OAuth/ambient credentials return None.
    pub const fn key_env(self) -> Option<&'static str> {
        match self {
            Self::AmazonBedrock => Some("AWS_BEARER_TOKEN_BEDROCK"),
            Self::AntLing => Some("ANT_LING_API_KEY"),
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::AzureOpenAiResponses => Some("AZURE_OPENAI_API_KEY"),
            Self::Baseten => Some("BASETEN_API_KEY"),
            Self::Cerebras => Some("CEREBRAS_API_KEY"),
            Self::CloudflareAiGateway => Some("CLOUDFLARE_AI_GATEWAY_API_KEY"),
            Self::CloudflareWorkersAi => Some("CLOUDFLARE_API_TOKEN"),
            Self::Cursor => Some("CURSOR_ACCESS_TOKEN"),
            Self::DatabricksUnityGateway => Some("DATABRICKS_TOKEN"),
            Self::Deepseek => Some("DEEPSEEK_API_KEY"),
            Self::Fireworks => Some("FIREWORKS_API_KEY"),
            Self::GithubCopilot => Some("COPILOT_GITHUB_TOKEN"),
            Self::Google => Some("GEMINI_API_KEY"),
            Self::GoogleVertex => Some("GOOGLE_CLOUD_API_KEY"),
            Self::Groq => Some("GROQ_API_KEY"),
            Self::Huggingface => Some("HF_TOKEN"),
            Self::KimiCoding => Some("KIMI_API_KEY"),
            Self::Meta => Some("META_API_KEY"),
            Self::Minimax => Some("MINIMAX_API_KEY"),
            Self::MinimaxCn => Some("MINIMAX_CN_API_KEY"),
            Self::Mistral => Some("MISTRAL_API_KEY"),
            Self::Moonshotai => Some("MOONSHOT_API_KEY"),
            Self::MoonshotaiCn => Some("MOONSHOT_API_KEY"),
            Self::Nvidia => Some("NVIDIA_API_KEY"),
            Self::OpenAi => Some("OPENAI_API_KEY"),
            Self::OpenAiCodex => None,
            Self::Opencode => Some("OPENCODE_API_KEY"),
            Self::OpencodeGo => Some("OPENCODE_API_KEY"),
            Self::Openrouter => Some("OPENROUTER_API_KEY"),
            Self::QwenTokenPlan => Some("QWEN_TOKEN_PLAN_API_KEY"),
            Self::QwenTokenPlanCn => Some("QWEN_TOKEN_PLAN_CN_API_KEY"),
            Self::QwenTokenPlanIndividual => Some("QWEN_TOKEN_PLAN_API_KEY"),
            Self::Radius => Some("RADIUS_API_KEY"),
            Self::SnowflakeCortex => Some("SNOWFLAKE_PAT"),
            Self::Together => Some("TOGETHER_API_KEY"),
            Self::VercelAiGateway => Some("AI_GATEWAY_API_KEY"),
            Self::Xai => Some("XAI_API_KEY"),
            Self::Xiaomi => Some("XIAOMI_API_KEY"),
            Self::XiaomiTokenPlanAms => Some("XIAOMI_TOKEN_PLAN_AMS_API_KEY"),
            Self::XiaomiTokenPlanCn => Some("XIAOMI_TOKEN_PLAN_CN_API_KEY"),
            Self::XiaomiTokenPlanSgp => Some("XIAOMI_TOKEN_PLAN_SGP_API_KEY"),
            Self::Zai => Some("ZAI_API_KEY"),
            Self::ZaiCodingCn => Some("ZAI_CODING_CN_API_KEY"),
            Self::Vercel => Some("AI_GATEWAY_API_KEY"),
            Self::Cheaperinference => Some("CHEAPERINFERENCE_API_KEY"),
            Self::Local => None,
        }
    }
    pub const fn default_model(self) -> &'static str {
        match self {
            Self::AmazonBedrock => "amazon.nova-2-lite-v1:0",
            Self::AntLing => "Ling-2.6-1T",
            Self::Anthropic => "claude-haiku-4-5",
            Self::AzureOpenAiResponses => "gpt-4o-mini",
            Self::Baseten => "deepseek-ai/DeepSeek-V4-Flash-0731",
            Self::Cerebras => "gpt-oss-120b",
            Self::CloudflareAiGateway => "claude-haiku-4.5",
            Self::CloudflareWorkersAi => "@cf/deepseek-ai/deepseek-v4-flash-0731",
            Self::Cursor => "auto",
            Self::DatabricksUnityGateway => "system.ai.claude-sonnet-4-6",
            Self::Deepseek => "deepseek-flash",
            Self::Fireworks => "accounts/fireworks/models/deepseek-v4-flash-0731",
            Self::GithubCopilot => "claude-sonnet-4.6",
            Self::Google => "gemini-2.5-flash",
            Self::GoogleVertex => "gemini-2.5-flash",
            Self::Groq => "llama-3.1-8b-instant",
            Self::Huggingface => "MiniMaxAI/MiniMax-M2",
            Self::KimiCoding => "k3",
            Self::Meta => "muse-spark-1.1",
            Self::Minimax => "MiniMax-M2.7",
            Self::MinimaxCn => "MiniMax-M2.7",
            Self::Mistral => "mistral-small-latest",
            Self::Moonshotai => "kimi-k2.6",
            Self::MoonshotaiCn => "kimi-k2.6",
            Self::Nvidia => "google/gemma-3-12b-it",
            Self::OpenAi => "gpt-4o-mini",
            Self::OpenAiCodex => "gpt-5.4",
            Self::Opencode => "claude-haiku-4-5",
            Self::OpencodeGo => "minimax-m3",
            Self::Openrouter => "openrouter/auto",
            Self::QwenTokenPlan => "MiniMax-M2.5",
            Self::QwenTokenPlanCn => "MiniMax-M2.5",
            Self::QwenTokenPlanIndividual => "deepseek-v4-flash-0731",
            Self::Radius => "balanced",
            Self::SnowflakeCortex => "claude-sonnet-4-5",
            Self::Together => "MiniMaxAI/MiniMax-M2.7",
            Self::VercelAiGateway => "alibaba/qwen-3-14b",
            Self::Xai => "grok-4.3",
            Self::Xiaomi => "mimo-v2.5",
            Self::XiaomiTokenPlanAms => "mimo-v2.5",
            Self::XiaomiTokenPlanCn => "mimo-v2.5",
            Self::XiaomiTokenPlanSgp => "mimo-v2.5",
            Self::Zai => "glm-4.7",
            Self::ZaiCodingCn => "glm-5.3-flash",
            Self::Vercel => "anthropic/claude-haiku-4.5",
            Self::Cheaperinference => "anthropic/claude-haiku-4.5",
            Self::Local => "qwen3.5:9b",
        }
    }
    pub fn protocol(self) -> Protocol {
        self.route(self.default_model()).protocol
    }

    /// Exact catalog match; unknown/custom IDs inherit the provider default route.
    pub fn route(self, model: &str) -> Route {
        for line in include_str!("models.tsv").lines() {
            let mut parts = line.split('\t');
            if parts.next() != Some(self.id()) || parts.next() != Some(model) {
                continue;
            }
            let _name = parts.next();
            let api = parts.next().unwrap_or("");
            let base_url = parts.next().unwrap_or("");
            return Route {
                protocol: protocol_for(api),
                base_url,
            };
        }
        Route {
            protocol: match self {
                Self::AmazonBedrock => Protocol::Bedrock,
                Self::AntLing => Protocol::ChatCompletions,
                Self::Anthropic => Protocol::Anthropic,
                Self::AzureOpenAiResponses => Protocol::Responses,
                Self::Baseten => Protocol::ChatCompletions,
                Self::Cerebras => Protocol::ChatCompletions,
                Self::CloudflareAiGateway => Protocol::Anthropic,
                Self::CloudflareWorkersAi => Protocol::ChatCompletions,
                Self::Cursor => Protocol::Cursor,
                Self::DatabricksUnityGateway => Protocol::Anthropic,
                Self::Deepseek => Protocol::ChatCompletions,
                Self::Fireworks => Protocol::Anthropic,
                Self::GithubCopilot => Protocol::Anthropic,
                Self::Google => Protocol::Google,
                Self::GoogleVertex => Protocol::Vertex,
                Self::Groq => Protocol::ChatCompletions,
                Self::Huggingface => Protocol::ChatCompletions,
                Self::KimiCoding => Protocol::Anthropic,
                Self::Meta => Protocol::Responses,
                Self::Minimax => Protocol::Anthropic,
                Self::MinimaxCn => Protocol::Anthropic,
                Self::Mistral => Protocol::ChatCompletions,
                Self::Moonshotai => Protocol::ChatCompletions,
                Self::MoonshotaiCn => Protocol::ChatCompletions,
                Self::Nvidia => Protocol::ChatCompletions,
                Self::OpenAi => Protocol::Responses,
                Self::OpenAiCodex => Protocol::Codex,
                Self::Opencode => Protocol::Anthropic,
                Self::OpencodeGo => Protocol::Anthropic,
                Self::Openrouter => Protocol::ChatCompletions,
                Self::QwenTokenPlan => Protocol::ChatCompletions,
                Self::QwenTokenPlanCn => Protocol::ChatCompletions,
                Self::QwenTokenPlanIndividual => Protocol::ChatCompletions,
                Self::Radius => Protocol::PiMessages,
                Self::SnowflakeCortex => Protocol::Anthropic,
                Self::Together => Protocol::ChatCompletions,
                Self::VercelAiGateway => Protocol::Anthropic,
                Self::Xai => Protocol::Responses,
                Self::Xiaomi => Protocol::ChatCompletions,
                Self::XiaomiTokenPlanAms => Protocol::ChatCompletions,
                Self::XiaomiTokenPlanCn => Protocol::ChatCompletions,
                Self::XiaomiTokenPlanSgp => Protocol::ChatCompletions,
                Self::Zai => Protocol::ChatCompletions,
                Self::ZaiCodingCn => Protocol::ChatCompletions,
                Self::Vercel => Protocol::ChatCompletions,
                Self::Cheaperinference => Protocol::ChatCompletions,
                Self::Local => Protocol::ChatCompletions,
            },
            base_url: self.base_url(),
        }
    }

    /// Resolve provider URL templates. Overrides replace the complete base URL.
    /// For model-specific URLs, use `route(model).resolve_base_url(override_url)`.
    pub fn resolve_base_url(self, override_url: Option<&str>) -> Result<String, ModelError> {
        resolve_url(self.id(), self.base_url(), override_url)
    }

    pub fn models(self) -> Vec<ModelInfo> {
        let mut models: Vec<ModelInfo> = include_str!("models.tsv")
            .lines()
            .filter_map(|line| {
                let mut p = line.split('\t');
                if p.next()? != self.id() {
                    return None;
                }
                let id = p.next()?.to_owned();
                let name = p.next()?.to_owned();
                let _api = p.next()?;
                let _url = p.next()?;
                let context_length = p
                    .next()
                    .and_then(|n| n.parse::<u64>().ok())
                    .filter(|n| *n > 0);
                Some(ModelInfo {
                    id,
                    name: Some(name),
                    context_length,
                    pricing: None,
                    reasoning: None,
                })
            })
            .collect();
        // Provider switching uses the first catalog entry when no model is
        // saved. Keep that aligned with the intentional startup default.
        if let Some(index) = models
            .iter()
            .position(|model| model.id == self.default_model())
        {
            let default = models.remove(index);
            models.insert(0, default);
        } else {
            models.insert(
                0,
                ModelInfo {
                    id: self.default_model().into(),
                    name: None,
                    context_length: None,
                    pricing: None,
                    reasoning: None,
                },
            );
        }
        models
    }
}

impl Route {
    pub fn resolve_base_url(
        self,
        provider: ProviderPreset,
        override_url: Option<&str>,
    ) -> Result<String, ModelError> {
        resolve_url(provider.id(), self.base_url, override_url)
    }
}

fn protocol_for(api: &str) -> Protocol {
    match api {
        "anthropic-messages" => Protocol::Anthropic,
        "openai-responses" | "azure-openai-responses" => Protocol::Responses,
        "openai-codex-responses" => Protocol::Codex,
        "google-generative-ai" => Protocol::Google,
        "google-vertex" => Protocol::Vertex,
        "bedrock-converse-stream" => Protocol::Bedrock,
        "cursor-agent" => Protocol::Cursor,
        "pi-messages" => Protocol::PiMessages,
        _ => Protocol::ChatCompletions,
    }
}

fn resolve_url(
    provider: &str,
    template: &str,
    override_url: Option<&str>,
) -> Result<String, ModelError> {
    if let Some(url) = override_url {
        return validate_url(provider, url);
    }
    let mut url = template.to_owned();
    if provider == "amazon-bedrock" {
        // An explicit region wins over the catalog's model-specific region.
        let region = ["AWS_REGION", "AWS_DEFAULT_REGION"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .find(|region| !region.trim().is_empty());
        if let Some(region) = region {
            validate_segment(provider, "AWS region", region.trim())?;
            url = format!("https://bedrock-runtime.{}.amazonaws.com", region.trim());
        }
    }
    if provider == "google-vertex" {
        // Vertex needs a project even though it does not appear in the API root.
        required_env(provider, "GOOGLE_CLOUD_PROJECT")?;
    }
    if url.is_empty() && provider == "azure-openai-responses" {
        url = required_env(provider, "AZURE_OPENAI_ENDPOINT")?;
        // ProviderModel adds /openai/v1 for a bare endpoint, but a query
        // must remain after the path (ResponsesModel preserves it there).
        if let Some((root, query)) = url.split_once('?') {
            if !root.contains("/openai/") {
                url = format!("{}/openai/v1?{query}", root.trim_end_matches('/'));
            }
        }
    }
    if provider == "google-vertex" && url.contains("{location}") {
        let location = std::env::var("GOOGLE_CLOUD_LOCATION")
            .ok()
            .filter(|s| !s.trim().is_empty())
            // Match ProviderModel's default project path; global is also a
            // valid Vertex location and uses the unprefixed hostname.
            .unwrap_or_else(|| "us-central1".to_owned());
        validate_segment(provider, "GOOGLE_CLOUD_LOCATION", &location)?;
        url = if location == "global" {
            url.replace("{location}-", "")
        } else {
            url.replace("{location}", &location)
        };
    }
    for (placeholder, env) in [
        ("{CLOUDFLARE_ACCOUNT_ID}", "CLOUDFLARE_ACCOUNT_ID"),
        ("{CLOUDFLARE_GATEWAY_ID}", "CLOUDFLARE_GATEWAY_ID"),
        ("{DATABRICKS_HOST}", "DATABRICKS_HOST"),
        ("{SNOWFLAKE_CORTEX_BASE_URL}", "SNOWFLAKE_CORTEX_BASE_URL"),
    ] {
        if url.contains(placeholder) {
            let value = required_env(provider, env)?;
            if matches!(env, "CLOUDFLARE_ACCOUNT_ID" | "CLOUDFLARE_GATEWAY_ID") {
                validate_segment(provider, env, &value)?;
            }
            url = url.replace(placeholder, value.trim_end_matches('/'));
        }
    }
    validate_url(provider, &url)
}

pub(crate) fn validate_segment(provider: &str, name: &str, value: &str) -> Result<(), ModelError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err(ModelError::Request(format!(
            "{provider}: invalid {name}; expected a single identifier, not a URL or path"
        )));
    }
    Ok(())
}

fn required_env(provider: &str, name: &str) -> Result<String, ModelError> {
    std::env::var(name)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            ModelError::Request(format!(
                "{provider} requires {name}; set {name} or supply an explicit base URL"
            ))
        })
}

fn validate_url(provider: &str, url: &str) -> Result<String, ModelError> {
    let url = url.trim();
    if !(url.starts_with("https://") || url.starts_with("http://"))
        || url.contains('{')
        || url.contains('}')
        || url.chars().any(char::is_whitespace)
        || url.ends_with("://")
    {
        return Err(ModelError::Request(format!("invalid {provider} base URL; supply a complete HTTP(S) URL (and configure required environment variables)")));
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| {
        ModelError::Request(format!(
            "invalid {provider} base URL: expected a complete HTTP(S) URL"
        ))
    })?;
    if parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || (parsed.query().is_some()
            && (provider != "azure-openai-responses"
                || parsed.query_pairs().count() != 1
                || !parsed
                    .query_pairs()
                    .all(|(key, value)| key == "api-version" && !value.is_empty())))
    {
        return Err(ModelError::Request(format!(
            "invalid {provider} base URL: remove credentials, fragment or unsupported query"
        )));
    }
    Ok(url.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_defaults_and_catalog_routes() {
        assert_eq!(ALL.len(), 47);
        let mut seen = HashSet::new();
        for &preset in ALL {
            assert!(seen.insert(preset.id()));
            assert_eq!(ProviderPreset::from_id(preset.id()), Some(preset));
            assert!(!preset.default_model().is_empty());
            if !matches!(
                preset,
                ProviderPreset::Vercel
                    | ProviderPreset::Cheaperinference
                    | ProviderPreset::Local
                    | ProviderPreset::Openrouter
                    | ProviderPreset::OpenAiCodex
            ) {
                assert!(
                    preset
                        .models()
                        .iter()
                        .any(|m| m.id == preset.default_model()),
                    "{}",
                    preset.id()
                );
            }
        }
        assert_eq!(ProviderPreset::from_id("nonexistent"), None);
        assert_eq!(ProviderPreset::OpenAi.default_model(), "gpt-4o-mini");
        assert_eq!(ProviderPreset::Local.default_model(), "qwen3.5:9b");
    }

    #[test]
    fn mixed_routes_and_config() {
        let db = ProviderPreset::DatabricksUnityGateway;
        assert_eq!(
            db.route("system.ai.claude-sonnet-4-6").protocol,
            Protocol::Anthropic
        );
        assert_eq!(db.route("system.ai.glm-5-2").protocol, Protocol::Responses);
        assert_eq!(
            db.route("system.ai.glm-5-2")
                .resolve_base_url(db, Some("https://example.com/v1"))
                .unwrap(),
            "https://example.com/v1"
        );
        let snow = ProviderPreset::SnowflakeCortex;
        assert_eq!(
            snow.route("claude-sonnet-4-5").protocol,
            Protocol::Anthropic
        );
        assert_eq!(
            snow.route("openai-gpt-5").protocol,
            Protocol::ChatCompletions
        );
        assert_eq!(
            ProviderPreset::GithubCopilot
                .route("claude-fable-5")
                .protocol,
            Protocol::Anthropic
        );
        assert_eq!(
            ProviderPreset::GithubCopilot.route("gpt-6-sol").protocol,
            Protocol::Responses
        );
        assert!(validate_url("test", "{PLACEHOLDER}/v1").is_err());
        assert!(validate_url("test", "https://example.com/?secret=x").is_err());
        assert!(required_env("test", "THIS_VARIABLE_SHOULD_NOT_EXIST_4839").is_err());
    }

    #[test]
    fn conversational_defaults_and_fallback_roots() {
        assert_eq!(ProviderPreset::Google.default_model(), "gemini-2.5-flash");
        assert_eq!(
            ProviderPreset::AzureOpenAiResponses.default_model(),
            "gpt-4o-mini"
        );
        assert_eq!(
            ProviderPreset::GoogleVertex.route("custom").protocol,
            Protocol::Vertex
        );
        assert_eq!(
            ProviderPreset::Anthropic.route("custom").base_url,
            "https://api.anthropic.com"
        );
        assert_eq!(
            ProviderPreset::Mistral.route("custom").base_url,
            "https://api.mistral.ai"
        );
        assert_eq!(
            ProviderPreset::Openrouter.route("custom").base_url,
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(
            ProviderPreset::OpenAi.route("custom").protocol,
            Protocol::Responses
        );
        assert_eq!(
            ProviderPreset::OpenAiCodex.route("custom").protocol,
            Protocol::Codex
        );
        for provider in [
            ProviderPreset::Google,
            ProviderPreset::OpenAi,
            ProviderPreset::Openrouter,
        ] {
            for model in provider.models() {
                assert!(!model.id.contains("deep-research"));
                assert!(!model.id.contains("realtime"));
            }
        }
    }

    #[test]
    fn azure_query_is_the_only_accepted_query() {
        assert_eq!(
            validate_url(
                "azure-openai-responses",
                "https://example.com/openai/v1/?api-version=preview"
            )
            .unwrap(),
            "https://example.com/openai/v1/?api-version=preview"
        );
        for url in [
            "https://example.com/?api-version=",
            "https://example.com/?api-version=x&other=y",
            "https://example.com/?other=x",
        ] {
            assert!(validate_url("azure-openai-responses", url).is_err());
        }
        assert!(validate_url("openai", "https://example.com/?api-version=preview").is_err());
    }
}
