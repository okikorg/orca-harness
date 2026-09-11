# `orca-harness-sdk`

`orca-harness-sdk` is the high-level, CLI-independent Rust facade for composing Orca Harness applications. It re-exports the stable building blocks most hosts need and supplies builders for agents, harnesses, runs, sessions, tools, memory, MCP, skills, and extensions.

## Main API

- `Harness` and `HarnessBuilder` assemble models, tools, extensions, callbacks, and host configuration.
- `Agent` and `AgentBuilder` define reusable agent behavior and tool presets.
- `RunRequest`, `RunHandle`, `RunResult`, `RunOutcome`, `RunEvent`, and `EventCallback` start runs, consume the bounded event stream or cancellation, and report detailed outcomes with partial usage on failure.
- `Sessions`, `SessionBuilder`, and `Session` support persistent, forkable session workflows.
- `Memory` configures explicit global/workspace memory; `Skills` manages skill sources and destinations.
- `Mcp` reports MCP server status and coordinates MCP integration; `SkillPreview` inspects a skill before it is installed.
- `TruncationConfig`, `RetryConfig` (with `ToolRetryOptions` and `ModelRetryOptions`), and `Compaction` configure common extensions without exposing host internals.
- `SubagentConfig` and `Subagents` spawn and control detached child agents; `ProcessConfig` and `Processes` start and watch background OS processes; `Workflows` runs a `WorkflowSubmission` of `Stage`s over the same subagents.
- `BackgroundNotification`, read through `Session::notifications`, reports every worker, stage, and process exit; `Session::shutdown` ends a session's detached work explicitly.

`Agent::run` is the one-shot entry point: it opens an ephemeral session, runs one request, and drops the session when the call returns, so no history carries between calls and overlapping calls are allowed. Use a `Session` when you need multi-turn history, persistence, or control over work that should outlive a single run.

The facade also re-exports core contracts, model adapters, provider credentials, web integrations, and standard tool bundles so a typical host needs one workspace dependency instead of wiring every crate manually.

The top level is a convenience subset: the most common companion types for implementing `Model`, `Tool`, `Extension`, and `CredentialSource` (such as `ModelResponse`, `ToolContext`, `ToolResult`, `ToolDecision`, `HarnessError`, and the Codex credential contract), plus the standard tool bundle from `orca-harness-tools` (`core_tools*`, `fs_admin_tools`, `Workspace`, `FileGuard`, `Executor`, and the ask, REPL, kernel, and todo tools), which has no namespace of its own.

Each of the four namespaces below is the complete grouping for its area:

- `contracts`: core traits plus the complete set of companion types needed to implement them.
- `providers`: model adapters, the model catalog, and credentials including Codex.
- `orchestration`: subagent, background process, and workflow handles.
- `integrations`: MCP, skills, web, memory, session recording, and reusable extension handles (events, policy, usage, retries, truncation, compaction).

`tests/consumer_api.rs` is the baseline: it depends on `orca_harness_sdk` alone and implements every contract. Use `orcacode` when you want the reference terminal host rather than a Rust embedding API.

```bash
cargo test -p orca-harness-sdk
cargo run -p orca-harness-sdk --example agent_tour
cargo run -p orca-harness-sdk --example host_assembly
```

Additional examples cover cancellation (`background_cancellation`), custom events, local MCP, memory and skills, session lifecycle, and recovery through compaction and retries. The orchestration examples are deterministic and need no credentials: `detached_subagents`, `background_processes`, `workflows`, and `partial_outcomes`.

## Orchestration

Two different things run "in the background". A `RunHandle` (from `Session::start`) is one parent run executing off the caller's task; it belongs to the caller, who reads its events and awaits its outcome. Subagents, background processes, and workflows are detached work owned by the session: they outlive the run that started them, and a session hands out `Subagents`, `Processes`, and `Workflows` handles to spawn, inspect, and cancel them from the host.

A detached subagent's result (and a workflow's run-level outcome) is owed to the parent conversation: it is delivered as one user turn at the parent's next model call, either inside a running turn or when the host calls `Session::continue_run(RunRequest::continuation())`. While the session is idle the host is told through `Session::notifications` (`CompletionsReady`, `SubagentFinished`, `ProcessNotified`); the session never starts a run on its own. `Session::shutdown` cancels the detached work and waits up to a grace period for it to exit; dropping the session cancels the same work but waits for nothing.

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
