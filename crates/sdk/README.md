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

The facade also re-exports core contracts, model adapters, provider credentials, web integrations, and standard tool bundles so a typical host needs one workspace dependency instead of wiring every crate manually. Use `orcacode` when you want the reference terminal host rather than a Rust embedding API.

```bash
cargo test -p orca-harness-sdk
cargo run -p orca-harness-sdk --example agent_tour
cargo run -p orca-harness-sdk --example host_assembly
```

Additional examples cover cancellation, custom events, local MCP, memory and skills, session lifecycle, and recovery through compaction and retries.

## Workspace role

Use this crate when building a Rust application that wants a supported composition surface rather than assembling every lower-level crate directly. It is a facade, not a second runtime: runs still execute through `orca-harness-core`, and optional tools and integrations remain subject to the host's policies.

Related documentation: the workspace [crate diagram](../../docs/crate-diagram.md).
