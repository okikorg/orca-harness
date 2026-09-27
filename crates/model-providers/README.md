# `orca-harness-model-providers`

Remote model adapters implement the unchanged `orca-harness-core::Model` contract. They stream deltas, tool calls and usage where supported; provider-specific authentication and wire formats remain in the adapters. The host still owns sessions, tools and approval policy.

## Choosing an adapter

- `ProviderModel` selects a registry preset and routes a model ID to the appropriate protocol. See [provider IDs, catalog semantics, credentials and endpoint requirements](PROVIDERS.md).
- Native `AnthropicModel`, `OpenAiModel`, `OpenRouterModel` and `OpenAiCodexModel` remain available. Anthropic Messages supports API-key auth, text/images, streamed tool calls, automatic prompt caching and paginated discovery; top-level `oneOf`/`anyOf`/`allOf` in tool input schemas are sanitized, nested occurrences preserved. No `thinking` key is sent; signed thinking blocks replay with their originating assistant turn. Hosted tools are not enabled. OpenAI and OpenRouter support OpenAI-compatible chat/completions with configurable roots. Codex uses subscription credentials and its own Responses protocol.
- Native `GoogleModel`, `ResponsesModel`, `BedrockModel`, `PiMessagesModel`, `CursorModel` and `CopilotModel` expose protocol-specific configuration. `ResponsesModel` is API-key-backed, not the Codex subscription adapter. `GoogleModel` supports explicit API key or bearer token (including Vertex) but does not acquire ADC credentials. Bedrock supports bearer auth only; Copilot exchanges a supplied GitHub token, not browser login; Cursor accepts an access token, not a login flow.
- `catalog` exports `ModelInfo`, `Pricing`, `ReasoningCapabilities` and `SupportedEfforts` for presenting provider-reported metadata. Model discovery is a network operation using the selected endpoint and credentials, not an embedded model list. Unreported metadata remains unknown; a listed model does not guarantee invocation permission.

```rust
use orca_harness_model_providers::{ProviderModel, ProviderPreset};

let preset = ProviderPreset::from_id("google").expect("known preset");
// SDK/host supplies the secret; ProviderModel does not read key_env() for you.
let model = ProviderModel::new(preset, "gemini-2.5-flash")
    .api_key(std::env::var("GEMINI_API_KEY")?)
    .max_tokens(2048);
// Pass `model` wherever an orca_harness_core::Model is required.
let _ = model;
```

The builder also offers `.base_url(...)`, `.protocol(...)`, `.reasoning_effort(...)`, `.user_agent(...)`, `.prompt_cache(...)`, `.session_id(...)` and `.attribution(...)`; each applies where the selected protocol supports it, and effort support is model/provider-specific. `.models()` lists the live catalog and `.context_window()` resolves the selected model's window, best effort. The selected adapter is retained across turns for provider-specific continuation state. For a custom model ID, check its route and set an explicit URL when needed. `ProviderPreset::ALL` contains all supported IDs. CLI selection uses `--provider ID` or `ORCA_PROVIDER=ID`, with `--model`, `--base-url` / `ORCA_BASE_URL`, and `--api-key` where applicable; CLI credential resolution is distinct from the SDK builder. See [PROVIDERS.md](PROVIDERS.md) for required Azure, Cloudflare, Databricks, Snowflake, Vertex and Bedrock configuration.

```bash
cargo test -p orca-harness-model-providers
cargo run -p orca-harness-model-providers --example stream_probe
```

These are local test/example commands, not a claim that expanded provider endpoints were live-tested. Related crates: [`orca-harness-core`](../harness-core), [`orca-harness-provider-auth`](../provider-auth) (credential boundary), and [`orca-harness-sdk`](../sdk) (root and `providers` reexports).
