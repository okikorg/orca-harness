# `orca-harness-extensions`

`orca-harness-extensions` contains reusable lifecycle components for production hosts. Each extension subscribes only to the core hooks it needs, so hosts can add observability, safety, durability, or context management without changing the agent loop.

## Extensions

- `EventStream` and `HarnessEvent` publish typed run, model, tool, and error events to a sink.
- `ToolPolicy` applies allow/deny decisions before a tool executes.
- `Truncation` caps large tool results; `TruncationStore` and `ReadToolResultTool` let a model page through preserved originals.
- `ToolRetry` and `RetryModel` retry eligible tool or transient model failures.
- `UsageMeter` aggregates model-reported token usage through a `UsageHandle`.
- `SessionHandler` records append-only JSONL transcripts and loads sessions for resume/fork workflows.
- `MemoryExtension`, `MemorySearchTool`, and `MemoryManageTool` provide explicit global/workspace memory storage backed by SQLite.
- `LongSession` and `compact` manage context growth using discovered model capacity.

`EventStream` emits `tool_call` before policy/review and `tool_finished` after completion. Register `execution_marker()` after approval, sandbox, and retry wrappers when hosts need separate preflight and execution timing. `ToolRetry::retry_error_when` can exclude deterministic or non-idempotent failures.

```bash
cargo test -p orca-harness-extensions
cargo run -p orca-harness-extensions --example compact_demo
```

## Workspace role

Extensions decorate the core lifecycle without becoming part of the kernel. Register only the extensions a host needs and order wrappers deliberately when policy, approval, retry, truncation, and execution timing interact. Persistent session and memory features require the host to choose their storage location and lifecycle.

Related crate: [`orca-harness-core`](../harness-core).
