//! Provider presets as data. Each provider is one row in `presets!` below;
//! routing, credentials, discovery and adapter construction read that row
//! instead of branching on provider IDs. Model availability is discovered live.
use orca_harness_core::ModelError;

/// A wire dialect. Several providers share each one.
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

impl Protocol {
    pub const ALL: &'static [Self] = &[
        Self::ChatCompletions,
        Self::Anthropic,
        Self::Responses,
        Self::Codex,
        Self::Google,
        Self::Vertex,
        Self::Bedrock,
        Self::Cursor,
        Self::PiMessages,
    ];

    /// The stable spelling used on command lines and in saved routes.
    pub const fn name(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat-completions",
            Self::Anthropic => "anthropic",
            Self::Responses => "responses",
            Self::Codex => "codex",
            Self::Google => "google",
            Self::Vertex => "vertex",
            Self::Bedrock => "bedrock",
            Self::Cursor => "cursor",
            Self::PiMessages => "pi-messages",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.name() == name)
    }

    /// Whether requests can cap output tokens.
    pub const fn supports_max_tokens(self) -> bool {
        !matches!(self, Self::Codex | Self::Cursor)
    }
}

/// How a preset obtains its credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    /// No credential (a local server).
    None,
    /// Subscription OAuth supplied by the host through a credential source.
    OAuth,
    /// An API key, conventionally read by the host from `env`.
    ApiKey {
        env: &'static str,
        placement: KeyPlacement,
    },
}

/// Where the API key goes on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPlacement {
    /// The adapter's native scheme (Bearer, `x-api-key`, `x-goog-api-key`, ...).
    Native,
    /// `Authorization: Bearer` on a Messages-compatible gateway.
    Bearer,
    /// A named header instead of the adapter's native scheme.
    Header(&'static str),
    /// Cloudflare AI Gateway's `cf-aig-authorization`, with upstream keys stored in the gateway.
    CloudflareGateway,
}

/// Which adapter wraps the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    /// The protocol's generic adapter.
    Protocol,
    /// OpenRouter's Chat Completions extensions (attribution, sessions, cache).
    OpenRouter,
    /// GitHub Copilot's token exchange in front of Chat Completions.
    Copilot,
}

/// The model-listing interface a provider documents. Transport compatibility
/// alone is not evidence of discovery support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discovery {
    Unsupported,
    /// `GET {root}/models` returning `{"data": [...]}`.
    OpenAiModels,
    /// OpenAI Models plus Ollama's native `/api/show` context window.
    Ollama,
    OpenRouter,
    Vercel,
    CheaperInference,
    Radius,
    Anthropic,
    Google,
    Copilot,
    Cursor,
    Codex,
}

/// One environment-derived URL component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Var {
    /// The placeholder is `{env[0]}`; later names are fallbacks.
    pub env: &'static [&'static str],
    pub default: Option<&'static str>,
    pub kind: VarKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    /// A single identifier such as an account, project or region.
    Segment,
    /// A complete URL prefix such as a workspace host.
    Url,
}

/// Provider behavior that differs from the protocol's defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quirks {
    /// Chat Completions: echo `reasoning_content` back on tool-call turns.
    pub replay_reasoning_content: bool,
    /// Responses: request and replay encrypted reasoning items.
    pub encrypted_reasoning: bool,
    /// Chat Completions: send effort as `reasoning.effort`.
    pub nested_reasoning_effort: bool,
    /// A user agent the service requires; it takes precedence over the host's.
    pub user_agent: Option<&'static str>,
}

const QUIRKS: Quirks = Quirks {
    replay_reasoning_content: false,
    encrypted_reasoning: true,
    nested_reasoning_effort: false,
    user_agent: None,
};
const REPLAY: Quirks = Quirks {
    replay_reasoning_content: true,
    ..QUIRKS
};

/// Everything the crate knows about one provider.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub id: &'static str,
    /// Older IDs that still resolve to this preset.
    pub aliases: &'static [&'static str],
    /// The API root, with `{VAR}` placeholders filled from `vars`.
    pub base_url: &'static str,
    pub vars: &'static [Var],
    /// Appended to a root that came from a `Url` variable with no path.
    pub root_path: Option<&'static str>,
    /// The single query parameter a base URL may carry.
    pub query: Option<&'static str>,
    /// Post-processing for URL rules a template cannot express.
    pub url_hook: Option<fn(String) -> String>,
    pub credential: Credential,
    /// `None`: the service serves several dialects; callers must choose one.
    pub protocol: Option<Protocol>,
    pub adapter: Adapter,
    pub discovery: Discovery,
    /// Whether discovery works without a credential.
    pub public_catalog: bool,
    pub quirks: Quirks,
}

const SPEC: Spec = Spec {
    id: "",
    aliases: &[],
    base_url: "",
    vars: &[],
    root_path: None,
    query: None,
    url_hook: None,
    credential: Credential::None,
    protocol: Some(Protocol::ChatCompletions),
    adapter: Adapter::Protocol,
    discovery: Discovery::Unsupported,
    public_catalog: false,
    quirks: QUIRKS,
};

const fn key(env: &'static str) -> Credential {
    Credential::ApiKey {
        env,
        placement: KeyPlacement::Native,
    }
}

/// A Chat Completions provider with a native key and no discovery.
const fn chat(id: &'static str, base_url: &'static str, env: &'static str) -> Spec {
    Spec {
        id,
        base_url,
        credential: key(env),
        ..SPEC
    }
}

const fn segment(env: &'static [&'static str]) -> Var {
    Var {
        env,
        default: None,
        kind: VarKind::Segment,
    }
}

const fn url_var(env: &'static [&'static str]) -> Var {
    Var {
        env,
        default: None,
        kind: VarKind::Url,
    }
}

/// Vertex's `global` location uses the unprefixed host.
fn vertex_global_host(url: String) -> String {
    url.replacen("://global-aiplatform.", "://aiplatform.", 1)
}

macro_rules! presets {
    ($($variant:ident => $spec:expr,)*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum ProviderPreset { $($variant,)* }

        const SPECS: &[Spec] = &[$($spec,)*];

        /// Every preset, in display order.
        pub const ALL: &[ProviderPreset] = &[$(ProviderPreset::$variant,)*];
    };
}

presets! {
    OpenRouter => Spec {
        base_url: crate::openrouter::OPENROUTER_BASE_URL,
        adapter: Adapter::OpenRouter,
        discovery: Discovery::OpenRouter,
        public_catalog: true,
        ..chat("openrouter", "", "OPENROUTER_API_KEY")
    },
    Vercel => Spec {
        aliases: &["vercel-ai-gateway"],
        base_url: crate::vercel::VERCEL_GATEWAY_BASE_URL,
        discovery: Discovery::Vercel,
        public_catalog: true,
        quirks: Quirks { nested_reasoning_effort: true, ..QUIRKS },
        ..chat("vercel", "", "AI_GATEWAY_API_KEY")
    },
    CheaperInference => Spec {
        base_url: crate::cheaperinference::CHEAPERINFERENCE_BASE_URL,
        discovery: Discovery::CheaperInference,
        public_catalog: true,
        ..chat("cheaperinference", "", "CHEAPERINFERENCE_API_KEY")
    },
    OpenAi => Spec {
        protocol: Some(Protocol::Responses),
        discovery: Discovery::OpenAiModels,
        ..chat("openai", "https://api.openai.com/v1", "OPENAI_API_KEY")
    },
    OpenAiCodex => Spec {
        id: "openai-codex",
        base_url: crate::openai_codex::CODEX_BASE_URL,
        credential: Credential::OAuth,
        protocol: Some(Protocol::Codex),
        discovery: Discovery::Codex,
        ..SPEC
    },
    Anthropic => Spec {
        base_url: crate::anthropic::ANTHROPIC_BASE_URL,
        protocol: Some(Protocol::Anthropic),
        discovery: Discovery::Anthropic,
        ..chat("anthropic", "", "ANTHROPIC_API_KEY")
    },
    AmazonBedrock => Spec {
        base_url: "https://bedrock-runtime.{AWS_REGION}.amazonaws.com",
        vars: &[Var { default: Some("us-east-1"), ..segment(&["AWS_REGION", "AWS_DEFAULT_REGION"]) }],
        protocol: Some(Protocol::Bedrock),
        ..chat("amazon-bedrock", "", "AWS_BEARER_TOKEN_BEDROCK")
    },
    AntLing => chat("ant-ling", "https://api.ant-ling.com/v1", "ANT_LING_API_KEY"),
    AzureOpenAiResponses => Spec {
        base_url: "{AZURE_OPENAI_ENDPOINT}",
        vars: &[url_var(&["AZURE_OPENAI_ENDPOINT"])],
        root_path: Some("/openai/v1"),
        query: Some("api-version"),
        credential: Credential::ApiKey { env: "AZURE_OPENAI_API_KEY", placement: KeyPlacement::Header("api-key") },
        protocol: Some(Protocol::Responses),
        ..chat("azure-openai-responses", "", "")
    },
    Baseten => chat("baseten", "https://inference.baseten.co/v1", "BASETEN_API_KEY"),
    Cerebras => Spec {
        discovery: Discovery::OpenAiModels,
        ..chat("cerebras", "https://api.cerebras.ai/v1", "CEREBRAS_API_KEY")
    },
    CloudflareAiGateway => Spec {
        base_url: "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/anthropic/v1",
        vars: &[segment(&["CLOUDFLARE_ACCOUNT_ID"]), segment(&["CLOUDFLARE_GATEWAY_ID"])],
        credential: Credential::ApiKey { env: "CLOUDFLARE_AI_GATEWAY_API_KEY", placement: KeyPlacement::CloudflareGateway },
        protocol: Some(Protocol::Anthropic),
        ..chat("cloudflare-ai-gateway", "", "")
    },
    CloudflareWorkersAi => Spec {
        vars: &[segment(&["CLOUDFLARE_ACCOUNT_ID"])],
        ..chat("cloudflare-workers-ai", "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1", "CLOUDFLARE_API_TOKEN")
    },
    Cursor => Spec {
        base_url: crate::cursor::CURSOR_BASE_URL,
        protocol: Some(Protocol::Cursor),
        discovery: Discovery::Cursor,
        ..chat("cursor", "", "CURSOR_ACCESS_TOKEN")
    },
    DatabricksUnityGateway => Spec {
        base_url: "{DATABRICKS_HOST}/ai-gateway/anthropic/v1",
        vars: &[url_var(&["DATABRICKS_HOST"])],
        credential: Credential::ApiKey { env: "DATABRICKS_TOKEN", placement: KeyPlacement::Bearer },
        protocol: None,
        ..chat("databricks-unity-gateway", "", "")
    },
    Deepseek => Spec {
        discovery: Discovery::OpenAiModels,
        quirks: REPLAY,
        ..chat("deepseek", "https://api.deepseek.com", "DEEPSEEK_API_KEY")
    },
    Fireworks => chat("fireworks", "https://api.fireworks.ai/inference/v1", "FIREWORKS_API_KEY"),
    GithubCopilot => Spec {
        adapter: Adapter::Copilot,
        discovery: Discovery::Copilot,
        ..chat("github-copilot", "https://api.individual.githubcopilot.com", "COPILOT_GITHUB_TOKEN")
    },
    Google => Spec {
        base_url: crate::google::GOOGLE_BASE_URL,
        protocol: Some(Protocol::Google),
        discovery: Discovery::Google,
        ..chat("google", "", "GEMINI_API_KEY")
    },
    GoogleVertex => Spec {
        base_url: "https://{GOOGLE_CLOUD_LOCATION}-aiplatform.googleapis.com/v1/projects/{GOOGLE_CLOUD_PROJECT}/locations/{GOOGLE_CLOUD_LOCATION}/publishers/google",
        vars: &[
            segment(&["GOOGLE_CLOUD_PROJECT"]),
            Var { default: Some("us-central1"), ..segment(&["GOOGLE_CLOUD_LOCATION"]) },
        ],
        url_hook: Some(vertex_global_host),
        protocol: Some(Protocol::Vertex),
        ..chat("google-vertex", "", "GOOGLE_CLOUD_API_KEY")
    },
    Groq => Spec {
        discovery: Discovery::OpenAiModels,
        ..chat("groq", "https://api.groq.com/openai/v1", "GROQ_API_KEY")
    },
    Huggingface => chat("huggingface", "https://router.huggingface.co/v1", "HF_TOKEN"),
    KimiCoding => Spec {
        protocol: Some(Protocol::Anthropic),
        quirks: Quirks { user_agent: Some("orcacode"), ..QUIRKS },
        ..chat("kimi-coding", "https://api.kimi.com/coding/v1", "KIMI_API_KEY")
    },
    Meta => Spec {
        protocol: Some(Protocol::Responses),
        quirks: Quirks { encrypted_reasoning: false, ..QUIRKS },
        ..chat("meta", "https://api.meta.ai/v1", "META_API_KEY")
    },
    Minimax => Spec {
        protocol: Some(Protocol::Anthropic),
        ..chat("minimax", "https://api.minimax.io/anthropic/v1", "MINIMAX_API_KEY")
    },
    MinimaxCn => Spec {
        protocol: Some(Protocol::Anthropic),
        ..chat("minimax-cn", "https://api.minimaxi.com/anthropic/v1", "MINIMAX_CN_API_KEY")
    },
    Mistral => Spec {
        discovery: Discovery::OpenAiModels,
        ..chat("mistral", "https://api.mistral.ai/v1", "MISTRAL_API_KEY")
    },
    Moonshotai => Spec {
        quirks: REPLAY,
        ..chat("moonshotai", "https://api.moonshot.ai/v1", "MOONSHOT_API_KEY")
    },
    MoonshotaiCn => Spec {
        quirks: REPLAY,
        ..chat("moonshotai-cn", "https://api.moonshot.cn/v1", "MOONSHOT_API_KEY")
    },
    Nvidia => chat("nvidia", "https://integrate.api.nvidia.com/v1", "NVIDIA_API_KEY"),
    Opencode => Spec {
        protocol: None,
        ..chat("opencode", "https://opencode.ai/zen", "OPENCODE_API_KEY")
    },
    OpencodeGo => Spec {
        protocol: None,
        ..chat("opencode-go", "https://opencode.ai/zen/go", "OPENCODE_API_KEY")
    },
    QwenTokenPlan => chat("qwen-token-plan", "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1", "QWEN_TOKEN_PLAN_API_KEY"),
    QwenTokenPlanCn => chat("qwen-token-plan-cn", "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1", "QWEN_TOKEN_PLAN_CN_API_KEY"),
    QwenTokenPlanIndividual => chat("qwen-token-plan-individual", "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1", "QWEN_TOKEN_PLAN_API_KEY"),
    Radius => Spec {
        base_url: crate::pi_messages::RADIUS_BASE_URL,
        protocol: Some(Protocol::PiMessages),
        discovery: Discovery::Radius,
        public_catalog: true,
        ..chat("radius", "", "RADIUS_API_KEY")
    },
    SnowflakeCortex => Spec {
        base_url: "{SNOWFLAKE_CORTEX_BASE_URL}",
        vars: &[url_var(&["SNOWFLAKE_CORTEX_BASE_URL"])],
        credential: Credential::ApiKey { env: "SNOWFLAKE_PAT", placement: KeyPlacement::Bearer },
        protocol: None,
        ..chat("snowflake-cortex", "", "")
    },
    Together => chat("together", "https://api.together.ai/v1", "TOGETHER_API_KEY"),
    Xai => Spec {
        protocol: Some(Protocol::Responses),
        quirks: Quirks { encrypted_reasoning: false, ..QUIRKS },
        ..chat("xai", "https://api.x.ai/v1", "XAI_API_KEY")
    },
    Xiaomi => chat("xiaomi", "https://api.xiaomimimo.com/v1", "XIAOMI_API_KEY"),
    XiaomiTokenPlanAms => chat("xiaomi-token-plan-ams", "https://token-plan-ams.xiaomimimo.com/v1", "XIAOMI_TOKEN_PLAN_AMS_API_KEY"),
    XiaomiTokenPlanCn => chat("xiaomi-token-plan-cn", "https://token-plan-cn.xiaomimimo.com/v1", "XIAOMI_TOKEN_PLAN_CN_API_KEY"),
    XiaomiTokenPlanSgp => chat("xiaomi-token-plan-sgp", "https://token-plan-sgp.xiaomimimo.com/v1", "XIAOMI_TOKEN_PLAN_SGP_API_KEY"),
    Zai => Spec {
        quirks: REPLAY,
        ..chat("zai", "https://api.z.ai/api/coding/paas/v4", "ZAI_API_KEY")
    },
    ZaiCodingCn => Spec {
        quirks: REPLAY,
        ..chat("zai-coding-cn", "https://open.bigmodel.cn/api/coding/paas/v4", "ZAI_CODING_CN_API_KEY")
    },
    Local => Spec {
        id: "local",
        base_url: "http://localhost:11434/v1",
        discovery: Discovery::Ollama,
        public_catalog: true,
        ..SPEC
    },
}

/// A resolved transport: dialect plus API root template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub protocol: Protocol,
    pub base_url: &'static str,
}

impl ProviderPreset {
    pub const ALL: &'static [Self] = ALL;

    pub const fn spec(self) -> &'static Spec {
        &SPECS[self as usize]
    }

    /// Resolve a preset ID or one of its aliases.
    pub fn from_id(id: &str) -> Option<Self> {
        ALL.iter()
            .copied()
            .find(|p| p.id() == id || p.spec().aliases.contains(&id))
    }

    pub const fn id(self) -> &'static str {
        self.spec().id
    }

    /// The API root template. Placeholders are resolved by [`Self::resolve_base_url`].
    pub const fn base_url(self) -> &'static str {
        self.spec().base_url
    }

    /// Credential lookup is the host's responsibility; OAuth/ambient credentials return None.
    pub const fn key_env(self) -> Option<&'static str> {
        match self.spec().credential {
            Credential::ApiKey { env, .. } => Some(env),
            Credential::OAuth | Credential::None => None,
        }
    }

    /// A provider-only route. Mixed dialect services require explicit selection.
    pub fn route(self) -> Result<Route, ModelError> {
        Ok(Route {
            protocol: self.protocol()?,
            base_url: self.base_url(),
        })
    }

    pub fn protocol(self) -> Result<Protocol, ModelError> {
        self.spec().protocol.ok_or_else(|| {
            ModelError::Request(format!(
                "{} has multiple transports; set ProviderModel::protocol(...) and an explicit base_url for that interface",
                self.id()
            ))
        })
    }

    /// Resolve the URL template from the environment. An override replaces the complete root.
    pub fn resolve_base_url(self, override_url: Option<&str>) -> Result<String, ModelError> {
        let spec = self.spec();
        if let Some(url) = override_url {
            return validate_url(spec, url);
        }
        let mut url = spec.base_url.to_owned();
        let mut from_url_var = false;
        for var in spec.vars {
            let placeholder = format!("{{{}}}", var.env[0]);
            if !url.contains(&placeholder) {
                continue;
            }
            let value = var_value(spec.id, var)?;
            match var.kind {
                VarKind::Segment => validate_segment(spec.id, var.env[0], &value)?,
                VarKind::Url => from_url_var |= url.starts_with(&placeholder),
            }
            url = url.replace(&placeholder, value.trim_end_matches('/'));
        }
        if let (true, Some(path)) = (from_url_var, spec.root_path) {
            url = with_root_path(&url, path);
        }
        if let Some(hook) = spec.url_hook {
            url = hook(url);
        }
        validate_url(spec, &url)
    }
}

fn var_value(provider: &str, var: &Var) -> Result<String, ModelError> {
    var.env
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
        .or_else(|| var.default.map(str::to_owned))
        .ok_or_else(|| {
            let name = var.env[0];
            ModelError::Request(format!(
                "{provider} requires {name}; set {name} or supply an explicit base URL"
            ))
        })
}

/// Add `path` to a bare endpoint, keeping any query after it.
fn with_root_path(url: &str, path: &str) -> String {
    let (root, query) = match url.split_once('?') {
        Some((root, query)) => (root, Some(query)),
        None => (url, None),
    };
    let bare = reqwest::Url::parse(root).is_ok_and(|u| matches!(u.path(), "" | "/"));
    if !bare {
        return url.to_owned();
    }
    let root = format!("{}{path}", root.trim_end_matches('/'));
    match query {
        Some(query) => format!("{root}?{query}"),
        None => root,
    }
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

fn validate_url(spec: &Spec, url: &str) -> Result<String, ModelError> {
    let provider = spec.id;
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
    let query_ok = match (parsed.query(), spec.query) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(_), Some(allowed)) => {
            parsed.query_pairs().count() == 1
                && parsed
                    .query_pairs()
                    .all(|(key, value)| key == allowed && !value.is_empty())
        }
    };
    if parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || !query_ok
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

    #[test]
    fn table_rows_match_their_variants() {
        for (index, preset) in ALL.iter().enumerate() {
            assert_eq!(*preset as usize, index);
            assert!(!preset.id().is_empty());
            assert_eq!(ProviderPreset::from_id(preset.id()), Some(*preset));
        }
        let mut ids: Vec<_> = ALL
            .iter()
            .flat_map(|p| std::iter::once(p.id()).chain(p.spec().aliases.iter().copied()))
            .collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "preset IDs and aliases must be unique");
    }

    #[test]
    fn every_template_placeholder_has_a_variable() {
        for preset in ALL {
            let spec = preset.spec();
            let mut url = spec.base_url.to_owned();
            for var in spec.vars {
                url = url.replace(&format!("{{{}}}", var.env[0]), "x");
            }
            assert!(!url.contains('{'), "{} has an unbound placeholder", spec.id);
            if let Credential::ApiKey { env, .. } = spec.credential {
                assert!(!env.is_empty(), "{} has an empty key variable", spec.id);
            }
        }
    }

    #[test]
    fn protocol_names_roundtrip() {
        for protocol in Protocol::ALL {
            assert_eq!(Protocol::from_name(protocol.name()), Some(*protocol));
        }
        assert_eq!(Protocol::from_name("openai"), None);
    }

    #[test]
    fn bare_endpoint_gains_root_path_before_query() {
        assert_eq!(
            with_root_path("https://x.example?api-version=1", "/openai/v1"),
            "https://x.example/openai/v1?api-version=1"
        );
        assert_eq!(
            with_root_path("https://x.example/openai/v1", "/openai/v1"),
            "https://x.example/openai/v1"
        );
    }
}
