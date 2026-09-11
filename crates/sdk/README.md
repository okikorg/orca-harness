# `orca-harness-sdk`

`orca-harness-sdk` is the high-level, CLI-independent Rust facade for composing Orca Harness applications. It re-exports the stable building blocks most hosts need and supplies builders for agents, harnesses, runs, sessions, tools, memory, MCP, skills, and extensions.

## Main API

- `Harness` and `HarnessBuilder` assemble models, tools, extensions, callbacks, and host configuration.
- `Agent` and `AgentBuilder` define reusable agent behavior and tool presets.
- `RunRequest`, `RunHandle`, `RunResult`, and `EventCallback` start runs and consume streamed events or cancellation.
- `Sessions`, `SessionBuilder`, and `Session` support persistent, forkable session workflows.
- `Memory` configures explicit global/workspace memory; `Skills` manages skill sources and destinations.
- `Mcp` reports MCP server status and coordinates MCP integration.
- `TruncationConfig`, `RetryConfig`, and `Compaction` configure common extensions without exposing host internals.

The facade also re-exports core contracts, model adapters, provider credentials, web integrations, and standard tool bundles so a typical host needs one workspace dependency instead of wiring every crate manually. The companion types needed to implement `Model`, `Tool`, `Extension`, and `CredentialSource` (such as `ModelResponse`, `ToolContext`, `ToolResult`, `HarnessError`, and the Codex credential contract) are exported at the top level, and the wider surface is grouped into four namespaces:

- `contracts`: core traits plus every companion type needed to implement them.
- `providers`: model adapters, the model catalog, and credentials including Codex.
- `orchestration`: subagent, background process, and workflow handles.
- `integrations`: MCP, skills, web, memory, session recording, and reusable extension handles.

`tests/consumer_api.rs` is the baseline: it depends on `orca_harness_sdk` alone and implements every contract. Use `orcacode` when you want the reference terminal host rather than a Rust embedding API.

```bash
cargo test -p orca-harness-sdk
cargo run -p orca-harness-sdk --example agent_tour
cargo run -p orca-harness-sdk --example host_assembly
```

Additional examples cover cancellation, custom events, local MCP, memory and skills, session lifecycle, and recovery through compaction and retries.

## Anthropic

The SDK re-exports the native Messages API adapter. Supply the API key explicitly;
the SDK does not read environment variables on your behalf.

```rust,no_run
use orca_harness_sdk::{AnthropicModel, Harness, RunRequest};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let model = AnthropicModel::new("claude-haiku-4-5")
    .api_key(std::env::var("ANTHROPIC_API_KEY")?)
    .max_tokens(8192)
    .prompt_cache(true);
let harness = Harness::builder().workspace(".").build()?;
let agent = harness.agent(model).system_prompt("You are a helpful assistant.").build()?;
let result = agent.run(RunRequest::new("Hello")).await?;
# Ok(())
# }
```

Caching is opt-in in the SDK and uses Anthropic's automatic ephemeral cache.
`Usage` exposes uncached input, cache reads, and cache creation separately.
`base_url` accepts an API root including `/v1`; `models()` fetches the native
catalog. `reasoning_effort` sets `output_config.effort` for models supporting it.
No `thinking` key is sent, so each model applies its own default; a model that
thinks streams reasoning as `ModelDelta::Reasoning`, and its signed thinking
blocks are replayed on the assistant turn that produced them. Provider-hosted
tools are not supported in this version.

In the CLI, use `orcacode --anthropic -p "Hello"` with `ANTHROPIC_API_KEY`, or select
`anthropic` through `/provider`. The CLI enables prompt-cache hints by default;
`--no-prompt-cache` disables them.

Protocol references: [Messages streaming](https://platform.claude.com/docs/en/build-with-claude/streaming)
and [prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching).

## Workspace role

Use this crate when building a Rust application that wants a supported composition surface rather than assembling every lower-level crate directly. It is a facade, not a second runtime: runs still execute through `orca-harness-core`, and optional tools and integrations remain subject to the host's policies.

Related documentation: the workspace [crate diagram](../../docs/crate-diagram.md).
