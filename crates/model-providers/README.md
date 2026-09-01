# `orca-harness-model-providers`

`orca-harness-model-providers` adapts remote model APIs to the `orca-harness-core::Model` contract. Provider modules preserve a shared streaming and message shape where possible while keeping provider-specific request, response, and authentication behavior isolated.

## Included adapters

- `OpenAiModel` for OpenAI-compatible chat/completions endpoints, including configurable base URLs.
- `OpenRouterModel` for OpenRouter; it reuses the OpenAI-compatible protocol and adds OpenRouter request metadata.
- `OpenAiCodexModel` for the OpenAI Codex Responses protocol and credential-based device-login integration.
- `catalog` types (`ModelInfo`, `Pricing`, `ReasoningCapabilities`, and `SupportedEfforts`) for model discovery and capability presentation.

Adapters support streamed deltas, tool calls, usage reporting, and image data URLs where supported. Credentials are supplied through the provider-neutral `CredentialSource` boundary rather than stored by this crate.

```bash
cargo test -p orca-harness-model-providers
cargo run -p orca-harness-model-providers --example stream_probe
```

## Workspace role

Provider adapters sit between a remote API and the core `Model` trait. They do not run tools, own sessions, or decide approval policy; those concerns belong to the core, extensions, or host. Use the builder methods in each provider module to configure endpoints, credentials, streaming, and provider-specific options.

Related crates: [`orca-harness-core`](../harness-core) and [`orca-harness-provider-auth`](../provider-auth).
