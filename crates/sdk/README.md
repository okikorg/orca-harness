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

### End-to-end host assembly

Run `cargo run -p orca-harness-sdk --example host_assembly` for an offline,
assertion-backed release-checklist host. It demonstrates:

- Explicit workspace/state directories, memory CRUD and automatic recall, and
  isolated skill discovery, enable/disable, and instruction loading.
- Custom tools, an exercised publication-deny policy, tool retries, streamed
  `RunHandle` events, and usage accounting.
- A real local stdio MCP handshake, tool selection, and tool call. The example
  launches its own executable in a private server mode; no Python or Node is needed.
- Persistent session discovery and resume, recovery of truncated tool output,
  and manual compaction of an isolated session fork.
- Model retries, partial failure outcomes, and cooperative cancellation.
- Detached subagents with a dedicated model route, a dependency-and-map workflow,
  stored outputs, and delivery of both completion types to the parent conversation.
- Background process readiness and exit notifications, synchronized through stdin,
  followed by explicit session shutdown and MCP disconnection.

The example uses deterministic model doubles and temporary state that is removed
on exit. It requires a POSIX shell, but no API credentials or external services.
It demonstrates run-event streaming, not provider token streaming. Live providers,
credential refresh, HTTP MCP, web integrations, remote skill installation,
containers, and language kernels are outside its coverage; see the focused
examples under `examples/` for additional integration patterns.

Additional examples cover cancellation (`background_cancellation`), custom events, local MCP, memory and skills, session lifecycle, and recovery through compaction and retries. The orchestration examples (`detached_subagents`, `background_processes`, and `workflows`) and `partial_outcomes`, which demonstrates `RunOutcome` on runs that stop early, are deterministic and need no credentials.

The use-case examples under `examples/use_cases/` each define one agent for one
real job, and each opens with the agent definition itself - system prompt,
domain tools, and the host policy that bounds it - so the shape of an agent is
the first thing you read:

- `agent_triage`: an on-call responder that classifies an alert and may page the
  rotation only for a sev1, with the severity gate enforced by a `ToolPolicy`
  rather than by the prompt.
- `agent_support`: a multi-turn customer support session with order lookup,
  automatic memory recall of the customer's stated preference, and a refund
  ceiling the model cannot talk its way past.
- `agent_data_science`: an analyst over a CSV in the workspace, whose `group_by`
  and `describe` tools do real arithmetic under `ToolPreset::ReadOnly`, so every
  number in the answer was computed rather than narrated.

`agent_triage` and `agent_support` each run a second scenario in which the model
oversteps and the policy refuses, so the guardrail is visible and not just
described. All three are deterministic and need no credentials.

## Orchestration

Two different things run "in the background". A `RunHandle` (from `Session::start`) is one parent run executing off the caller's task; it belongs to the caller, who reads its events and awaits its outcome. Subagents, background processes, and workflows are detached work owned by the session: they outlive the run that started them, and a session hands out `Subagents`, `Processes`, and `Workflows` handles to spawn, inspect, and cancel them from the host.

A detached subagent's result (and a workflow's run-level outcome) is owed to the parent conversation: it is delivered as one user turn at the parent's next model call: inside the running turn if one is in progress, otherwise at the first model call of the host's next run (`continue_run` or `run`). While the session is idle the host is told through `Session::notifications` (`CompletionsReady`, `SubagentFinished`, `ProcessNotified`); the session never starts a run on its own. `Session::shutdown` cancels the detached work and waits up to a grace period for it to exit; dropping the session cancels the same work but waits for nothing.

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

### Live release-review host

`live_host_assembly` is a separate, auto-discovered example; `host_assembly`
remains deterministic and offline. Supply credentials in your environment, then
choose a provider and optionally a model ID:

```bash
# Requires ANTHROPIC_API_KEY
cargo run -p orca-harness-sdk --example live_host_assembly -- anthropic
# Requires OPENAI_API_KEY
cargo run -p orca-harness-sdk --example live_host_assembly -- openai gpt-5
# Requires OPENROUTER_API_KEY
cargo run -p orca-harness-sdk --example live_host_assembly -- openrouter anthropic/claude-sonnet-4.5
# Requires both CODEX_ACCESS_TOKEN and CODEX_ACCOUNT_ID (ChatGPT account credentials)
cargo run -p orca-harness-sdk --example live_host_assembly -- codex gpt-5-codex
```

Defaults are `claude-sonnet-4-5`, `gpt-5`, `anthropic/claude-sonnet-4.5`, and
`gpt-5-codex`, respectively. Model availability and tool-schema support depend on
your account/provider. Credentials are read only at runtime, never printed by the
example. Codex follows `providers/codex.rs`: refresh returns the supplied static
credentials; this is **not** a production token-refresh implementation.

**This makes real, potentially billable model calls**, including a dedicated
same-provider child adapter used by detached subagents and workflow stages. There
are no scripted fallback responses. A POSIX shell is required for the one allowed
process command. The model reviews a seeded `RELEASE.md` in a temporary workspace,
loads a local skill, calls a real local stdio MCP tool, delegates an independent
review, submits a two-stage dependency workflow, and checks fixture presence with
a harmless process. The MCP peer is a private mode of the same executable, not an
external dependency; its tag check is fixture evidence, not a registry lookup or
proof that tests passed.

The Coding preset is constrained by an inherited, default-deny tool policy:
read-oriented tools, orchestration, the local MCP check, and only the exact fixture
check process command are allowed. General shell execution, file mutations,
publishing and other external tool actions are denied. This is application policy,
not an OS sandbox; run only with trusted fixture inputs. Provider requests still
use the network. Memory recall is seeded by the host; skill loading and other
requested capabilities are verified from successful tool exchanges, not prose.

The host drains run events, prints per-parent-run usage (including partial usage
on failures), configures model/tool retries and output truncation, and records a
persistent session under temporary `.orca` state. It subscribes before spawning,
checks worker/workflow/process statuses, waits for completion-inbox delivery, and
drives further parent turns rather than shutting down at the first final answer.
It bounds execution to ten minutes, eight parent turns, 24 steps per parent turn,
and eight steps per child. Missing capability calls, detached failures, notification
gaps, failed workflow/process outcomes, and exhausted budgets are reported as
errors/partial reviews. Sessions shut down and MCP disconnects on fallible exit;
temporary workspace and persistent records are removed on exit, not retained for
later invocations.

Coverage is intentionally limited: this is not every integration. Retry/truncation
are configured, not guaranteed to trigger; memory recall is not an asserted model
memory-tool call. Event counts describe SDK run events, not provider token
streaming. Parent usage is not a combined cost ledger for all child calls. No
publication, web search, remote MCP, remote skills, credential refresh, compaction,
containers, kernels, or cross-invocation session resume is demonstrated. A model
may decline or misuse tools, in which case the example fails its checks rather
than claiming success. Compilation alone does not validate live provider behavior.
