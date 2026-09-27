# Provider presets and endpoints

`registry::ProviderPreset::ALL` contains **46 presets**. `vercel-ai-gateway` is an alias of `vercel`. Use the exact lowercase, hyphenated IDs below with `ProviderPreset::from_id` or CLI `--provider ID` / `ORCA_PROVIDER=ID`:

| Provider ID | Provider ID | Provider ID | Provider ID |
| --- | --- | --- | --- |
| `openrouter` | `vercel` | `cheaperinference` | `openai` |
| `openai-codex` | `anthropic` | `amazon-bedrock` | `ant-ling` |
| `azure-openai-responses` | `baseten` | `cerebras` | `cloudflare-ai-gateway` |
| `cloudflare-workers-ai` | `cursor` | `databricks-unity-gateway` | `deepseek` |
| `fireworks` | `github-copilot` | `google` | `google-vertex` |
| `groq` | `huggingface` | `kimi-coding` | `meta` |
| `minimax` | `minimax-cn` | `mistral` | `moonshotai` |
| `moonshotai-cn` | `nvidia` | `opencode` | `opencode-go` |
| `qwen-token-plan` | `qwen-token-plan-cn` | `qwen-token-plan-individual` | `radius` |
| `snowflake-cortex` | `together` | `xai` | `xiaomi` |
| `xiaomi-token-plan-ams` | `xiaomi-token-plan-cn` | `xiaomi-token-plan-sgp` | `zai` |
| `zai-coding-cn` | `local` | | |

The registry contains provider connection configuration, not model knowledge. It does not embed model IDs, context windows, or model-name-to-protocol tables.

## One row per provider

Everything the crate knows about a provider lives in its `Spec` row in `src/registry/mod.rs`: ID and aliases, API root template and its environment variables, credential and key placement, protocol, adapter, discovery interface, whether the catalog is public, and protocol quirks (reasoning replay, encrypted reasoning, nested effort, a required user agent). `ProviderModel` and the CLI read those fields; no code branches on a provider ID. Adding a provider is one row in the `presets!` table; the enum variant, `ALL` and lookup are generated from it.

Discovery support per preset:

| Discovery | Presets |
| --- | --- |
| OpenAI Models (`GET /models`) | `openai`, `deepseek`, `groq`, `mistral`, `cerebras`, `local` (plus Ollama `/api/show` for context windows) |
| Provider-specific catalog | `openrouter`, `vercel`, `cheaperinference`, `radius`, `anthropic`, `google`, `github-copilot`, `cursor`, `openai-codex` |
| Public (no key needed to browse) | `openrouter`, `vercel`, `cheaperinference`, `radius`, `local` |
| Unavailable | every other preset; supply `--model` |

## Dynamic discovery and routing

`ProviderModel::models().await` fetches the configured provider's catalog using the configured credentials. Results reflect what that API reports: missing context windows, pricing, or reasoning capabilities stay unknown. A successful empty catalog, an authentication error, and unavailable discovery are distinct outcomes; none substitutes a built-in list.

An explicit or saved model ID remains authoritative, even if discovery is unavailable or the listing omits an alias. Without a selected model, the CLI must obtain one from discovery or ask for an explicit `--model`.

Model enumeration does not necessarily advertise the inference protocol. Use provider-advertised endpoint metadata where available, a documented model-independent transport where applicable, or an explicit protocol and API root. Mixed-interface services must not infer a protocol from a model name. An explicit `base_url(...)` always replaces the complete root; it is never treated as a request for automatic routing merely because it equals a preset URL. Selecting a protocol other than the preset's own requires an explicit `base_url`, because the preset root serves only its own dialect.

Provider integration does not imply universal discovery support. Bedrock's bearer-only runtime adapter cannot substitute for AWS control-plane model/profile discovery with IAM credentials. Vertex and some gateways do not expose a discovery contract compatible with the implemented catalog readers. Those cases report discovery unavailable and require explicit configuration rather than fabricating model availability.

## Credentials and URL configuration

The host must pass credentials explicitly to `ProviderModel::api_key(...)` (or to a native adapter); `key_env()` is a hint for host/CLI lookup, **not** automatic environment lookup by `ProviderModel`. Never embed secrets in base URLs. `base_url(...)` replaces the preset URL; supply a complete HTTP(S) endpoint/root suitable for the selected protocol and model. URL-template environment variables below are used when no override is supplied:

| Preset | Required configuration without explicit URL override | Authentication caveat |
| --- | --- | --- |
| `azure-openai-responses` | `AZURE_OPENAI_ENDPOINT` (Azure Responses root; deployment/API-version configuration must match your endpoint) | `AZURE_OPENAI_API_KEY` is sent only as the `api-key` header. A bare endpoint gains `/openai/v1`; a single `api-version` query is allowed. |
| `cloudflare-ai-gateway` | `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_GATEWAY_ID` | `CLOUDFLARE_AI_GATEWAY_API_KEY` authenticates the gateway via `cf-aig-authorization: Bearer …`, not the upstream provider. Configure upstream credentials in the gateway; the preset does not accept a separate upstream key. |
| `cloudflare-workers-ai` | `CLOUDFLARE_ACCOUNT_ID` | `CLOUDFLARE_API_TOKEN`. |
| `databricks-unity-gateway` | `DATABRICKS_HOST` (complete HTTP(S) host) | `DATABRICKS_TOKEN`; Anthropic route uses bearer auth. |
| `snowflake-cortex` | `SNOWFLAKE_CORTEX_BASE_URL` (complete HTTP(S) root) | `SNOWFLAKE_PAT`; Anthropic route uses bearer auth. |
| `google-vertex` | `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_LOCATION` for default regional URL | Supply `GOOGLE_CLOUD_API_KEY` explicitly for preset usage; native `GoogleModel::bearer_token(...)` accepts an explicit bearer token. No ADC lookup. A custom Vertex URL must include the required project/location/publisher path. |
| `amazon-bedrock` | Default is `us-east-1`; set `AWS_REGION` or provide a region-specific runtime `base_url` for another region. | Supply `AWS_BEARER_TOKEN_BEDROCK` as bearer token via `api_key(...)`. **No AWS SDK credential chain, IAM profiles, access-key signing or SigV4** in this adapter. |

`github-copilot` uses a supplied `COPILOT_GITHUB_TOKEN` for a GitHub-token exchange; it does **not** perform browser login. `cursor` requires an explicit `CURSOR_ACCESS_TOKEN`; it does **not** perform login. `openai-codex` is different from API-key Responses: supply a `CodexCredentialSource` through `OpenAiCodexModel` or `ProviderModel::codex_credentials(...)` for subscription credentials. `local` may not need a credential but requires a reachable local service. For other presets inspect `key_env()` and your account's endpoint configuration. The CLI can read the corresponding environment variable or a stored credential, while SDK callers supply credentials themselves.

No provider endpoints or credentials are claimed to have been live-tested by this documentation.
