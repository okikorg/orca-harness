# Modular SDK orchestration and API completeness

Date: 2026-09-11
Status: Design approved for saving; implementation not started.

## Goal

Support single-agent execution, subagents, background processes, and workflows through a small, coherent Rust SDK that reuses existing runtimes and closes the identified API and lifecycle gaps.

## Approach

Keep the SDK responsible for configuration, composition, and session ownership—not scheduling or execution. Extract only reusable, UI-independent completion handling from the CLI, and expose typed operations from existing tool implementations where necessary. Deliver incrementally with compatibility tests and a working example for every new public capability.

## Evidence and existing implementation

- `crates/sdk/src/agent.rs`: reusable agent definitions currently retain tools and extensions; the public Agent API does not implement the README's `agent.run(...)` example.
- `crates/sdk/src/session.rs`: session lifecycle, persistence, tool/extension assembly, and execution share one module; opening creates a fresh context, every execution adds a user message, and errors return before constructing usage-bearing RunResult.
- `crates/sdk/src/run.rs`: RunHandle uses an unbounded event receiver; RunRequest does not accept external cancellation.
- `crates/sdk/src/lib.rs`: exposes implementation traits without all required companion types; only partial orchestration exports exist.
- `crates/sdk/src/skills.rs`: tool creation captures a snapshot; source installation with list_only discards candidate information.
- `crates/sdk/src/mcp.rs`: manager and catalog maintain related state, while agent construction captures tool instances.
- `crates/harness-core/src/agent.rs`: run_context already accepts caller-owned context and cancellation.
- `crates/tools/src/subagent.rs` and `subagent/background/manager.rs`: existing child execution, routing, admission, cancellation, notification, acknowledgement, and generation handling.
- `crates/tools/src/core_tools/process.rs`: existing process execution, limits, statistics, and notification callbacks.
- `crates/tools/src/workflow.rs`, `workflow/runtime.rs`, and `workflow/store.rs`: workflow execution over ordinary subagents and in-memory session-scoped outputs.
- `crates/harness-dag/src/lib.rs`: graph validation and deterministic scheduling state; no replacement engine is needed.
- `crates/cli/src/runtime/completions.rs`: bounded completion admission, acknowledgement, generation checks, batching, and wakeup coalescing, currently coupled to CLI messages.
- `crates/cli/src/runtime/interactive.rs`: reference integration of process notifications, subagents, child extensions, workflow storage, and completion delivery.

The preceding review ran `cargo test -p orca-harness-sdk`: all 12 integration tests passed, with zero doctests. This does not validate the proposed implementation. No additional test execution is part of saving this plan.

## Design boundaries: avoid SDK bloat

1. No second scheduler, process manager, retry engine, graph schema, or persistence format.
2. No dependency from SDK, tools, or extensions back to CLI.
3. No generic job framework merging processes, subagents, and workflows; their lifecycle semantics differ.
4. Keep run-event observation separate from session background notifications. Do not create a catch-all event bus.
5. Typed host operations and model tools must call the same implementation. Do not duplicate tool dispatch or implement host operations by constructing ad hoc ToolContext/JSON calls.
6. Prefer concrete types and the existing Arc<dyn Model> boundary over public generic orchestration machinery.
7. Do not introduce feature flags solely to organize source files. Measure dependencies before proposing build-time optionality.
8. Preserve custom tool/extension escape hatches. Explicitly shared custom Arc instances remain caller-owned; SDK-created mutable built-ins become session-owned.
9. Split by responsibility. Review production modules above roughly 300 lines without imposing arbitrary fragmentation.
10. Add only configuration exercised by a real supported use case. Expose existing advanced handles rather than mirror every lower-level option.

## Ownership model

```text
Harness
  workspace/state roots and explicitly shared integrations
Agent
  reusable model/configuration, tool/extension definitions
Session
  conversation, recorder, recovery store
  session-owned built-in tool state
  optional background services
    subagent manager
    process controller
    workflow store/controller
    completion inbox
Run
  request or continuation, cancellation/deadline
  events, accounting, terminal outcome
```

Current agent-owned mutable tool instances are a prerequisite concern: independent sessions must not accidentally share processes, child managers, todo state, REPL state, workflow outputs, or completion inboxes. Shared user-supplied tools remain supported explicitly.

## Proposed module structure

```text
crates/sdk/src/
  lib.rs                  concise exports
  agent.rs                reusable definitions and builder
  harness.rs              workspace/state roots
  error.rs
  run.rs                  requests, outcomes, handles
  session/
    mod.rs                public lifecycle
    execution.rs          core assembly and execution
    persistence.rs        session/recovery plumbing
  background/
    mod.rs                optional session-owned assembly
    subagents.rs          thin host adapter
    processes.rs          thin host adapter
    workflows.rs          thin host adapter
  tools.rs                presets and session tool construction
  extensions.rs           extension configuration
  mcp.rs
  memory.rs
  skills.rs
```

Create modules only when their functionality lands. Lower-level process/subagent modules may be split where the typed-operation extraction establishes an actual responsibility boundary.

## Public API direction

Names are provisional pending compile-tested consumer examples.

- Agent::run for documented one-shot execution, with explicit cleanup semantics.
- SessionBuilder::context for imported conversation history.
- Session::continue_run without an artificial user message.
- Request-level caller cancellation using appropriate child-token semantics.
- Detailed terminal outcomes with partial usage/transcript on failure, preserving simple existing convenience signatures where practical.
- Session-owned controllers conceptually accessed through session.subagents(), session.processes(), and session.workflows().
- Typed spawn/submit requests and relevant completion/control handles.
- Background notification access and explicit awaited session.shutdown().

Ordinary host operations must not require constructing ToolContext. Host-originated actions must have an explicit policy/admission contract and must not silently bypass applicable session restrictions.

## Lifecycle and delivery contract

| Operation | Intended behavior |
| --- | --- |
| End parent run | Detached session work may continue; end of turn is not shutdown. |
| Cancel parent run | Cancel that run, not independent detached jobs. |
| Cancel child/workflow/process | Affect that operation and its documented descendants. |
| Clear/reset | Cancel owned work, invalidate generations, clear workflow outputs and associated mutable session state. |
| Fork | Copy conversation/recovery; do not share live jobs, processes, inboxes, or workflow handles. |
| Resume from disk | Restore transcript/recovery, not live processes or in-memory jobs. |
| Explicit shutdown | Stop admission, cancel owned work, and await bounded cleanup where supported. |
| Drop | Best-effort cleanup, not a substitute for awaited shutdown. |
| Completion while running | Deliver at a safe model boundary before compaction and recording. |
| Completion while idle | Notify host; do not silently launch a billable model run. |

Hosts wanting automatic continuation can await notifications and explicitly continue the session. Do not add a permanent autonomous worker loop to the SDK.

Host observation and parent delivery are separate: receiving a host notification must not consume the completion intended for the parent transcript. Define acknowledgement carefully. Workflow stage completions settle bookkeeping and remain host-observable, but must not enter the parent transcript as independent subagent completions; deliver the workflow-level outcome instead.

## Planned diff overview

```diff
~ crates/sdk/src/lib.rs
+ complete grouped public contracts; preserve compatible existing exports
~ crates/sdk/src/agent.rs
+ reusable configuration and explicit session-owned built-in construction
- crates/sdk/src/session.rs
+ crates/sdk/src/session/{mod,execution,persistence}.rs
+ crates/sdk/src/background/{mod,subagents,processes,workflows}.rs
~ crates/tools/src/subagent* and core_tools/process* and workflow*
+ typed entry points sharing existing execution implementations
~ crates/cli/src/runtime/completions.rs
- reusable bookkeeping mixed with UI handling
+ CLI adapter over shared bookkeeping; retain UI presentation locally
```

## Implementation checklist

### Phase A: API baseline and session foundations

- [ ] Establish a consumer API baseline and complete exports. Modify `sdk/src/lib.rs`, `sdk/README.md`, `sdk/Cargo.toml`, and add a focused SDK-only consumer compile-test fixture. Export companion model/tool/extension/credential types and supported orchestration contracts through coherent namespaces. Verify a consumer with only SDK as its Orca dependency can implement custom contracts and configure Codex credentials.

- [ ] Fix documented one-shot execution. Modify `sdk/src/agent.rs`, `sdk/src/run.rs`, README, and executable documentation tests. Add the thin ephemeral-session convenience, or correct documentation if lifecycle constraints make it misleading. Specify what happens to detached work when the one-shot owner disappears. Verify the README compiles and cleanup leaves no inaccessible owned work.

- [ ] Split session responsibilities without behavioral changes. Replace `sdk/src/session.rs` with `sdk/src/session/mod.rs`, `execution.rs`, and `persistence.rs`. Preserve persistence formats and extension order. Verify existing lifecycle, recovery, compaction, streaming, and overlap tests pass unchanged.

- [ ] Move SDK-created mutable tool state to session construction. Modify `sdk/src/agent.rs`, `tools.rs`, `session/mod.rs`, and add isolation tests. Audit presets, file guards, todos, Python/Bun, and integration snapshots. Preserve explicit custom sharing. Address compatibility for the existing agent-level todo accessor explicitly, with migration/deprecation if required. Verify two sessions cannot see/control one another's built-in state and multi-turn state remains intact within a session.

- [ ] Add context import, continuation, and caller cancellation. Modify session modules, `sdk/src/run.rs`, `error.rs`, and lifecycle tests. Define system-prompt precedence, validate resumable transcript shapes, and avoid cancelling caller-owned parent tokens. Verify imported histories persist correctly, continuation adds no user prompt, limits/deadlines hold, and cancellation permits orderly persistence cleanup.

- [ ] Add partial outcomes and bounded event observation. Modify `sdk/src/run.rs`, `session/execution.rs`, `error.rs`, and event/outcome tests. Preserve partial accounting on failure/cancellation and report both execution and persistence failures when relevant. Keep authoritative results separate from streamed observations. Verify slow/no readers cannot cause unbounded growth or deadlock finish(), and overflow is explicit.

Event implementation constraint: callbacks are currently synchronous. Simply replacing the unbounded channel with a bounded channel is insufficient. Select and test a loss-aware bounded observation stream, or explicitly justify a breaking asynchronous delivery contract. Never block a synchronous callback waiting for the receiver. Terminal outcome remains authoritative even if observational events overflow.

### Phase B: Shared completion infrastructure and subagents

- [ ] Extract reusable completion bookkeeping from CLI. Modify `cli/src/runtime/completions.rs` and its tests; create a focused module alongside subagent infrastructure in `tools`, updating exports as needed. Share generation validation, bounded admission/acknowledgement, ordering, and wakeup coalescing. Keep UiMsg, WorkerCmd, presentation, and CLI notices local. Verify existing CLI completion tests and dependency direction; do not copy the algorithm into SDK.

- [ ] Expose typed subagent operations through existing execution. Modify `tools/src/subagent.rs` and relevant submodules, `sdk/src/background/subagents.rs`, and agent configuration. Share routing, admission, execution, and cancellation between host and model-tool entry points. Verify foreground/background execution, queues, routing, nested depth, and cancellation have parity.

- [ ] Integrate session-owned subagents and completion delivery. Modify `sdk/src/background/mod.rs`, session execution/lifecycle modules, and integration tests. Own the manager/inbox per session. Define policy inheritance and child extension factories; do not share parent recording/meter instances accidentally. Verify restrictions reach children, completions reach the parent once, stale generations are suppressed, and idle notifications do not launch model calls.

### Phase C: Background processes

- [ ] Expose a typed controller from existing process implementation. Modify `tools/src/core_tools/process.rs`, focused process submodules only as needed, `tools/src/lib.rs`, and `sdk/src/background/processes.rs`. Share spawn, stdin/EOF, output, completion, list, and termination logic with ProcessTool. Verify host/tool calls observe the same IDs/state; output is bounded and failed launches/cancellation clean up.

- [ ] Integrate process configuration with presets and session lifecycle. Modify `sdk/src/tools.rs`, background assembly, session lifecycle, and process examples/tests. Construct the configured tool once rather than creating then replacing a default process manager. Preserve explicit remote-executor semantics: current file tools stay local. Verify readiness/exit notifications, session isolation, clear/shutdown, and that shell-less/read-only presets do not acquire process execution.

### Phase D: Workflows

- [ ] Expose typed workflow operations over existing runtime. Modify `tools/src/workflow.rs`, `workflow/runtime.rs`, `workflow/store.rs`, exports, and `sdk/src/background/workflows.rs`. Reuse harness-dag graph validation and scheduling. Expose submission, inspection, stage output, cancellation, and existing reuse semantics without duplicating schemas. Verify invalid graphs fail before admission, plus dependency execution, maps, stage caps, timeouts, and reuse parity.

- [ ] Integrate workflows with session subagent services. Modify SDK background/session assembly and workflow integration tests. Share manager, routing, child policies, concurrency settings, and session store. Reject incompatible configuration rather than creating an independent worker system. Verify stage notifications are host-observable but suppressed from parent delivery, terminal delivery occurs correctly, cancellation settles children, and fork/resume/clear honor ownership.

### Phase E: Remaining facade gaps

- [ ] Fix skill preview and run-boundary refresh. Modify `sdk/src/skills.rs`, session tool assembly, and skill tests/examples. Return owned source-preview metadata that survives temporary checkout cleanup. Preserve immutable tool schemas within a core run; refresh between runs. Verify preview writes no installation, populated list requests return metadata, and enable/disable/reload affects the next run.

- [ ] Define MCP refresh consistently with skills. Modify `sdk/src/mcp.rs`, session tool assembly, and MCP tests. Audit duplicated manager/catalog state and capture current tools between runs. Specify disconnect/reconnect behavior for in-flight calls; do not promise arbitrary mid-run registry mutation. Verify new connections become callable next turn and replaced connections do not leave stale tool targets or misleading status.

- [ ] Separate tool/model retry configuration only where needed. Modify `sdk/src/extensions.rs`, agent/session assembly, and retry tests. Add outcome/error predicates and notifications first; expose existing advanced gate/live-control handles instead of duplicating retry internals. Preserve old configuration through forwarding/deprecation where possible. Verify non-retryable/non-idempotent cases are excluded, data failures are classified, and retry layers are not accidentally nested.

### Phase F: Documentation, compatibility, and modularity

- [ ] Add deterministic end-to-end orchestration examples. Create SDK examples for detached subagents, actual background OS processes, workflows, and partial outcomes; update README and example declarations only where necessary. Distinguish RunHandle background execution from subagents/processes. Verify examples build and deterministic examples run without external credentials.

- [ ] Run compatibility, security, dependency, and size review. Modify only affected tests/manifests/docs as required. Check public API migration, CLI adapters, session isolation, policy inheritance, cancellation races, and disabled-feature overhead. Verify no additional scheduler/runtime crate, no CLI dependency, no unexplained dependency additions, and no oversized catch-all orchestration module.

## Validation matrix

| Area | Required cases |
| --- | --- |
| Consumer compilation | Custom model/tool/extension/credentials with SDK-only Orca dependency |
| Sessions | Isolation, fork, resume, clear/reset, imported context |
| Runs | Success, model failure, step limit, deadline, external cancellation, partial usage |
| Event stream | Slow reader, no reader, dropped reader, finish without draining, overflow |
| Subagents | Foreground/background, queue limits, routing, nested policies, cancellation, stale completions |
| Processes | stdin/EOF, bounded output, readiness, exit, launch failure, termination/shutdown |
| Workflows | Dependencies, maps, graph errors, stage cap, timeout, cancellation, reuse |
| Completions | Active/idle parent, batching, acknowledgement, generation reset, stage suppression |
| Integrations | Skill/MCP between-run changes, retry classification |
| Compatibility | Existing SDK examples/tests and CLI completion tests |

Planned implementation-time commands:

```sh
cargo fmt --all -- --check
cargo test -p orca-harness-sdk
cargo test -p orca-harness-tools
cargo test -p orca-harness-extensions
cargo test -p orca-harness-dag
cargo check -p orca-harness-sdk --examples
```

Also run the CLI's focused completion tests, the SDK-only consumer fixture, and clippy for affected packages using repository conventions verified at implementation time. Recheck worktree status and existing diffs before any implementation; preserve unrelated changes.

## Blast radius and risks

- SDK public API: prefer additive entry points and compatibility forwarding; explicitly review event receiver changes and agent-level mutable-state accessors.
- Tools: extracting typed operations must preserve tool schemas, validation, cancellation, and execution semantics.
- CLI: only shared completion extraction and adapters are in scope, not migration of the CLI onto SDK.
- State: session-scoping changes built-in sharing behavior; fork/reset/resume must be tested before orchestration is exposed.
- Security: child policy inheritance and host-call admission must not become bypasses; worker output remains untrusted content.
- Persistence: retain existing transcript/recovery formats; partial outcome handling must distinguish execution from persistence failure.
- Events: bounded observations need explicit overflow semantics; correctness-critical completions need admission/acknowledgement rather than eviction.
- Cleanup: async shutdown and best-effort drop differ; avoid promises that live jobs resume from disk.
- Dependencies: use existing crates; any new direct dependency such as graph-type exposure requires explicit justification and manifest verification.

## Out of scope

- Distributed workers, remote job queues, or durable restoration of live jobs/processes.
- A second workflow engine, universal job abstraction, or automatic idle model execution.
- New UI, login flows, CLI config parsing, or full CLI migration onto SDK.
- Unrelated provider-hosted tools, structured output, or memory backends.
- Speculative feature-flag matrices, plugin registries, or general-purpose service containers.

## Delivery order

Complete session ownership foundations before public orchestration conveniences. Next share completion bookkeeping and integrate subagents, then processes and workflows. Close remaining facade gaps and finish with consumer examples, security/lifecycle regression tests, and dependency/source-size review.

Each phase should be reviewable independently. Do not proceed to implementation until plan mode is exited and implementation is authorized.
