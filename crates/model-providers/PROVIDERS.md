# Provider presets and endpoints

`registry::ProviderPreset::ALL` contains **47 IDs**, including legacy presets and aliases. The CLI shows **46 distinct entries** because `vercel-ai-gateway` aliases `vercel`. Use the exact lowercase, hyphenated IDs below with `ProviderPreset::from_id` or CLI `--provider ID` / `ORCA_PROVIDER=ID`:

| Provider ID | Provider ID | Provider ID | Provider ID |
| --- | --- | --- | --- |
| `amazon-bedrock` | `ant-ling` | `anthropic` | `azure-openai-responses` |
| `baseten` | `cerebras` | `cloudflare-ai-gateway` | `cloudflare-workers-ai` |
| `cursor` | `databricks-unity-gateway` | `deepseek` | `fireworks` |
| `github-copilot` | `google` | `google-vertex` | `groq` |
| `huggingface` | `kimi-coding` | `meta` | `minimax` |
| `minimax-cn` | `mistral` | `moonshotai` | `moonshotai-cn` |
| `nvidia` | `openai` | `openai-codex` | `opencode` |
| `opencode-go` | `openrouter` | `qwen-token-plan` | `qwen-token-plan-cn` |
| `qwen-token-plan-individual` | `radius` | `snowflake-cortex` | `together` |
| `vercel-ai-gateway` | `xai` | `xiaomi` | `xiaomi-token-plan-ams` |
| `xiaomi-token-plan-cn` | `xiaomi-token-plan-sgp` | `zai` | `zai-coding-cn` |

Legacy extras: `vercel`, `cheaperinference`, `local`.

The registry exposes `id()`, `from_id()`, `default_model()`, `key_env()`, `base_url()`, `protocol()`, `route(model)`, `resolve_base_url(override_url)` and `models()`. Routes select among OpenAI-compatible Chat Completions, Anthropic Messages, API-key Responses, Codex, native Google/Vertex, Bedrock ConverseStream, Cursor and Pi Messages; **protocol can vary by model within a provider**. Unknown model IDs fall back to the provider's default route. `models()` (including `ProviderModel::models()`) uses an embedded static catalog snapshot; it is not a live list of models available to your account. Catalog entries/defaults do not guarantee access, support for every capability, or successful calls. Use an adapter's live discovery where available if you need current account-specific results.

## Credentials and URL configuration

The host must pass credentials explicitly to `ProviderModel::api_key(...)` (or to a native adapter); `key_env()` is a hint for host/CLI lookup, **not** automatic environment lookup by `ProviderModel`. Never embed secrets in base URLs. `base_url(...)` replaces the preset URL; supply a complete HTTP(S) endpoint/root suitable for the selected protocol and model. URL-template environment variables below are used when no override is supplied:

| Preset | Required configuration without explicit URL override | Authentication caveat |
| --- | --- | --- |
| `azure-openai-responses` | `AZURE_OPENAI_ENDPOINT` (Azure Responses root; deployment/API-version configuration must match your endpoint) | `AZURE_OPENAI_API_KEY` passed as key; Azure uses `api-key` header. |
| `cloudflare-ai-gateway` | `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_GATEWAY_ID` | `CLOUDFLARE_AI_GATEWAY_API_KEY` authenticates the gateway via `cf-aig-authorization: Bearer …`, not the upstream provider. Configure upstream credentials in the gateway; the preset does not accept a separate upstream key. |
| `cloudflare-workers-ai` | `CLOUDFLARE_ACCOUNT_ID` | `CLOUDFLARE_API_TOKEN`. |
| `databricks-unity-gateway` | `DATABRICKS_HOST` (complete HTTP(S) host) | `DATABRICKS_TOKEN`; Anthropic route uses bearer auth. |
| `snowflake-cortex` | `SNOWFLAKE_CORTEX_BASE_URL` (complete HTTP(S) root) | `SNOWFLAKE_PAT`; Anthropic route uses bearer auth. |
| `google-vertex` | `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_LOCATION` for default regional URL | Supply `GOOGLE_CLOUD_API_KEY` explicitly for preset usage; native `GoogleModel::bearer_token(...)` accepts an explicit bearer token. No ADC lookup. A custom Vertex URL must include the required project/location/publisher path. |
| `amazon-bedrock` | Default is `us-east-1`; set `AWS_REGION` or provide a region-specific runtime `base_url` for another region. | Supply `AWS_BEARER_TOKEN_BEDROCK` as bearer token via `api_key(...)`. **No AWS SDK credential chain, IAM profiles, access-key signing or SigV4** in this adapter. |

`github-copilot` uses a supplied `COPILOT_GITHUB_TOKEN` for a GitHub-token exchange; it does **not** perform browser login. `cursor` requires an explicit `CURSOR_ACCESS_TOKEN`; it does **not** perform login. `openai-codex` is different from API-key Responses: supply a `CodexCredentialSource` through `OpenAiCodexModel` or `ProviderModel::codex_credentials(...)` for subscription credentials. `local` may not need a credential but requires a reachable local service. For other presets inspect `key_env()` and your account's endpoint configuration. The CLI can read the corresponding environment variable or a stored credential, while SDK callers supply credentials themselves.

No provider endpoints or credentials are claimed to have been live-tested by this documentation.
