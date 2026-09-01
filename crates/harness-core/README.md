# `orca-harness-core`

`orca-harness-core` is the small, host-agnostic execution kernel at the center of Orca Harness. It runs an agent loop that alternates model responses with tool calls while keeping the privileged path limited to model invocation, tool dispatch, cancellation, limits, result pairing, deterministic ordering, and extension hooks.

## What it provides

- `Agent` and `Context` for configuring a run and its conversation.
- `Model`, `ModelResponse`, `ModelDelta`, and `DeltaSink` for model integrations.
- `Tool`, `ToolRegistry`, `FnTool`, `ToolCall`, and `ToolResult` for capability contracts.
- `Dispatcher` for concurrent tool execution with per-tool concurrency, ordering, deadlines, and cancellation.
- `Extension` and `ExtensionRegistry` for policy, telemetry, persistence, retries, and other host concerns without modifying the loop.
- `Limits` and a re-exported `CancellationToken` for bounding work.
- `testing` helpers including scripted models and concurrency probes.

## Minimal example

```rust,no_run
use orca_harness_core::{Agent, FnTool};
use serde_json::json;

# async fn example(model: impl orca_harness_core::Model) -> Result<(), orca_harness_core::HarnessError> {
let echo = FnTool::new("echo", "Return the supplied value", json!({"type": "object"}),
    |input, _context| async move { Ok(input) });
let result = Agent::new(model).tool(echo).run("Use echo").await?;
# let _ = result;
# Ok(()) }
```

The crate deliberately does not choose a provider, persistence format, permission model, sandbox, or user interface. Compose those concerns through extensions or a host. See the workspace [crate diagram](../../docs/crate-diagram.md).

```bash
cargo test -p orca-harness-core
cargo run -p orca-harness-core --example fake_llm
cargo run -p orca-harness-core --example fanout_probe -- 100 300
```

## Workspace role

This is the foundation crate for the workspace. It has no dependency on another workspace crate, so it can be embedded by a host or used as the base for higher-level integrations. Add provider adapters, host tools, and persistence separately rather than coupling them to the kernel.

Related crates: [`orca-harness-model-providers`](../model-providers), [`orca-harness-tools`](../tools), and [`orca-harness-extensions`](../extensions).
