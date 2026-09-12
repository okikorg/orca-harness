# Bun programmatic tool calling: minimal edits, modular reuse

## Goal
Enable agents to call existing tools from `bun_repl` JavaScript through a thin bridge that reuses the existing dispatcher, without redesigning or weakening the core harness.

## Approach
Let JavaScript produce independent and dependent tool requests, collect ready requests into bounded batches, and route existing `ToolCall` values through `Dispatcher::execute`, resolving promises from existing `ToolResult` values. Keep transport and Bun lifecycle in `crates/tools`, and bind the current host's actual tools and extension instances at agent construction/run assembly rather than adding a global registry or invocation fields to `ToolContext`. First prove a conservative outer-serial/inner-parallel composition using existing `Concurrency::Serial`; if that cannot preserve execution and policy behavior, stop and propose the smallest specific core diff separately.

## Design constraints

- Minimal edits and maximum reuse take precedence over idealized per-call streaming or speculative extensibility.
- Baseline proposal changes **no production files in `crates/harness-core`**. In particular, leave `dispatcher.rs`, `agent_loop.rs`, `tool.rs`, and `extension.rs` unchanged.
- No JavaScript parser, new DAG, new scheduler, copied dispatcher logic, separate approval system, or provider-specific PTC protocol.
- Ordinary Bun instances remain unchanged unless explicitly wrapped for PTC. No automatic expansion of tools available to subagents.
- Accept batch-completion latency and existing output transformations in v1.
- Approval of a parent Bun call is not approval of its child calls. Plan mode continues to deny Bun execution.
- Native Bun is not a sandbox. This feature does not restrict native filesystem/network access or establish global ordering against external processes/background agents.

## Evidence and codebase overlay

| Existing location | Existing primitive / behavior | Reuse / proposed overlay |
| --- | --- | --- |
| `crates/tools/src/bun_repl.rs` | Persistent `bun repl`; `.load` source files; bounded stdout/stderr pumps; timeout/reset; process-group cleanup; `Concurrency::Keyed("bun_repl")` | Preserve runtime and output protocol. Add only an optional per-invocation bridge entry path; keep IPC separate from console output. |
| `crates/tools/src/bun_repl/tests.rs` | Existing runtime regression tests | Extend tests for bootstrap, transport, cleanup and unchanged plain Bun behavior. |
| `crates/harness-core/src/tool.rs` | `ToolCall`, `ToolResult`, `ToolContext`, cloneable `ToolRegistry`, `Concurrency` | Use types unchanged; host creates scoped child registry from actual registration inputs. |
| `crates/harness-core/src/dispatcher.rs` | `Dispatcher::execute`; policy preflight; keyed/multi-key/serial grouping; cancellation; transformed results | Invoke unchanged for child batches. Its synchronization is allocated per batch, not globally. |
| `crates/harness-core/src/extension.rs` | Cloneable `ExtensionRegistry`, ordered subscriptions, shared `Arc<dyn Extension>` instances | Register the same instances in the same order; never rebuild a separate permissive policy chain. |
| `crates/harness-core/src/agent.rs` | Core Agent owns private registries | Do not assume accessors exist. Host assembly must supply registration inputs once to both core Agent and PTC binding. |
| `crates/harness-core/src/agent_loop.rs` | Only top-level dispatcher results are appended to model context | Do not insert child results into context; no loop modification needed for this property. |
| `crates/extensions/src/events.rs` | `EventStream::before_tool`, execution marker, finish and result events | Child dispatcher already emits tool events. Reuse existing events rather than introducing another event framework. |
| `crates/extensions/src/truncation.rs` | Post-execution transformations, optional full-result store | Preserve hooks including redaction and truncation. Do not promise raw/unlimited child results. Existing reader tool can be used if registered. |
| `crates/sdk/src/session.rs` | Per-run assembly of tools, events, truncation reader, retry, recording and other extensions | Bind here after complete registration inputs exist, not only in `AgentBuilder::bun()`. |
| `crates/sdk/src/agent.rs` | `bun()` currently constructs a plain shared Bun tool | Keep plain API working; explicit PTC construction must carry a run-scoped binding, not mutate a global tool callback. |
| `crates/cli/src/runtime/interactive.rs` | CLI builds CoreAgent directly with ordered host gates and tools | Integrate here; SDK-only wiring would miss the interactive CLI. |
| `crates/cli/src/headless.rs` | Separate direct CoreAgent construction plus selected-tool filtering | Integrate using the same reusable binding helper; preserve selected-tool filtering and HeadlessGate. |
| `crates/cli/src/runtime/local_tools.rs` | Bun identity survives MCP rebuilds, resets replace its generation | Retain cached raw runtime; construct a fresh scoped wrapper for each agent build so old policy/catalog captures are not installed into the persistent runtime. |
| `crates/cli/src/approval.rs`, `crates/cli/src/mode.rs` | Human approval and live PlanGate | Reuse without new bypass paths; verify live mode changes between child batches. |
| `crates/tools/src/subagent.rs` | Existing factory-based tool/extension assembly and child cancellation | Reuse patterns, not its model loop. Adding Bun PTC to every worker is deferred. |

## Proposed execution flow

```text
Agent issues bun_repl(code)
  -> PTC-enabled wrapper (outer Concurrency::Serial)
  -> existing persistent Bun runtime
  -> injected orca.callTool(name, args)
  -> dedicated local IPC requests, scoped to this invocation
  -> bounded ready-request batch of ToolCall values
  -> existing Dispatcher::execute(actual tools, actual extensions, token, deadline, limit)
  -> transformed ToolResult values returned at batch completion
  -> promises resolve/reject by request ID
  -> JS continues and may produce another batch
  -> existing Bun output result returns to the model
```

### Scheduling: smallest safe composition to prove first

The PTC wrapper uses existing `Concurrency::Serial` to exclude ordinary sibling execution in the **same outer batch** while it runs. It executes only one child batch at a time; children within that batch use the existing dispatcher parallel limit and conflict grouping. The outer permit/lock is not reused by the inner dispatcher, so children should not wait on resources held by their parent; verify this at parallelism 1 rather than assuming it.

This deliberately avoids concurrent child batches and new cross-batch locking. It does not change dispatcher semantics, but it **does change scheduling for the explicitly PTC-enabled Bun wrapper**: even compute-only calls through that wrapper are outer-serial. Plain Bun stays keyed. Child parallelism counts executable children; the outer orchestration call also occupies its existing outer slot, so do not claim a new global permit accounting guarantee.

The protection covers execution, not every hook: outer preflight runs before execution, and some extension wrappers can hold their own locks or replay calls. Prove host extension reentrancy, retry behavior, and cleanup before enabling the composition. Independent agents, background tools and external processes retain existing coordination limits; do not describe outer Serial as a workspace-wide lock.

If outer-serial behavior is unacceptable, or the tests expose a deadlock/policy gap, stop this implementation path. A shared nested scheduler is a separate design, not hidden scope in this patch.

### API and result contract

```ts
const results = await Promise.all([
  orca.callTool("read_file", { path: "Cargo.toml" }),
  orca.callTool("read_file", { path: "crates/tools/Cargo.toml" }),
]);
console.log(results);
```

- JavaScript supplies decomposition. The bridge does not inspect `Promise.all` or source code.
- A documented event-loop flush collects ready requests; later requests wait for the active batch and join the next one.
- Success resolves with `ToolResult.output`; `is_error` rejects with identity and the structured error output. Do not reinterpret arbitrary tool-specific success payloads.
- `Promise.all` rejection does not roll back mutations or imply sibling cancellation. Agents may use `Promise.allSettled` when they need every result.
- Results include normal post-tool transformations; no separate raw-data bypass in v1.
- Only existing Bun captured output becomes the top-level result. The REPL may echo expression values; do not promise that console.log is the only possible capture source.
- Generate host child IDs using parent invocation identity plus a unique attempt/sequence. Existing event IDs provide correlation without changing the event enum initially.

### Scope, lifecycle and bounds

- Child registry is the host's actual permitted tool set with REPL recursion excluded. Do not infer authorization solely from model-visible schemas: MCP has hidden registered targets with its own selection checks.
- Start with no child access to other orchestration entry points that can recursively run PTC; explicitly restrict Bun/Python and workflow/subagent orchestration for v1 unless separately proven safe. Ordinary tools may keep their existing background behavior; cancellation is cooperative, not rollback of external effects.
- Preserve the same extension instances and order, including plan gates, approval, mutation preflight, automatic review, plugin hooks, retry and result transforms.
- No strong reference cycle from Bun -> registry -> Bun. Bind per invocation, omit orchestration entries in the child registry, and keep long-lived runtime state separate from authority.
- Scope IPC to one active call with a private local endpoint and invocation identity; reject stale, malformed, oversized and duplicate requests. stdout is never a control channel.
- Bound queue size, batch size, total calls and response bytes with internal constants initially; no configuration framework.
- Effective deadline is the earlier of Bun's execution timeout and parent run deadline. Race the **whole child dispatch future**, including approval preflight, against cancellation/deadline because dispatcher job guards alone do not guard all pre-hooks.
- Cancel/drain pending requests when code finishes, throws, times out, resets, the process exits, or the parent future is dropped. Late callbacks cannot acquire authority from a later invocation.
- Outer retries may replay code and side effects; preserve configured policy but explicitly test/document this existing retry consequence. Do not add automatic bridge retries.

## Planned module boundaries and diff preview

Names below describe the intended small overlay, not a generic plugin framework.

```diff
+++ crates/tools/src/bun_repl/bridge.rs
+ Dedicated IPC, bounded request framing/queue, JS bootstrap and request/result pairing.

+++ crates/tools/src/ptc.rs
+ PTC-enabled Bun wrapper: Serial outer classification, per-call lifecycle,
+ one active child Dispatcher::execute batch, existing registry/type reuse.
+ Small shared host-registration helper only if needed to supply identical
+ registration inputs to CoreAgent and the wrapper without duplicated lists.

--- crates/tools/src/bun_repl.rs
+++ crates/tools/src/bun_repl.rs
+ Optional scoped bridge execution entry; reuse spawn/output/reset/kill paths.
+ Keep plain constructor, schema and keyed execution behavior compatible.

--- crates/tools/src/lib.rs
+++ crates/tools/src/lib.rs
+ Export only concrete PTC construction/binding types needed by hosts.

--- crates/sdk/src/session.rs
+++ crates/sdk/src/session.rs
+ Assemble registration inputs once and bind PTC after per-run tools/hooks exist.

--- crates/sdk/src/agent.rs
+++ crates/sdk/src/agent.rs
+ Minimal explicit PTC opt-in while preserving plain bun().

--- crates/cli/src/runtime/interactive.rs
+++ crates/cli/src/runtime/interactive.rs
+ Use shared registration/binding helper; wrap cached Bun for the current build.

--- crates/cli/src/headless.rs
+++ crates/cli/src/headless.rs
+ Same binding path with existing selected-tool filters and gates.

--- crates/cli/src/main.rs
+++ crates/cli/src/main.rs
+ Concise PTC usage guidance only when the capability is available.
```

Conditional small edits: `crates/tools/Cargo.toml` only if the chosen IPC transport needs an additional Tokio feature already available in the workspace; SDK module/re-export glue if the opt-in needs it; local tool cache only if lifecycle tests show a necessary change. Do not add a dependency before checking existing facilities. No source-size cleanup unrelated to the feature.

## Implementation steps and verification

- [ ] **Establish the baseline and prove composition before building IPC.** Create `crates/tools/tests/ptc.rs` with fake tools/extensions exercising an outer Serial wrapper and inner unchanged dispatcher; inspect worktree status/diff before implementation. Verify parallelism 1 and N, Serial/Keyed/Keys conflicts, two PTC parents, mixed ordinary/PTC batches, cancellation during preflight, and wrapper-held locks/retry behavior. Stop if preserving these requires core changes; record the exact proposed exception in this plan before proceeding.
- [ ] **Bind registration inputs once.** Create the minimal host-binding portion of `crates/tools/src/ptc.rs` and export it from `crates/tools/src/lib.rs`; initially exercise it in `crates/tools/tests/ptc.rs`. Verify registration order, replacement semantics, same extension Arc identity/order, restricted child tool scope, and absence of reference cycles. Do not mirror manually maintained tool/policy lists across hosts or add core registry accessors merely for convenience.
- [ ] **Add the Bun transport.** Create `crates/tools/src/bun_repl/bridge.rs` and minimally modify `crates/tools/src/bun_repl.rs`; extend `crates/tools/src/bun_repl/tests.rs`. Verify real Bun top-level await, parallel request flushes, dependent follow-up batches, structured failures, separate stdout, frame limits, missing Bun diagnostics and unchanged plain REPL persistence/reset.
- [ ] **Connect transport to existing dispatch and lifecycle.** Complete `crates/tools/src/ptc.rs` and `crates/tools/tests/ptc.rs`. Verify each child passes normal hooks, only one child batch is active, normal redaction/truncation remains applied, unique event identity, effective deadlines and bounded bytes/calls. Test unawaited work, throw, disconnect, parent-future drop, retry, reset and stale callbacks; no bridge dispatch survives its invocation.
- [ ] **Integrate the SDK at per-run assembly.** Modify `crates/sdk/src/session.rs`, `crates/sdk/src/agent.rs`, and only necessary SDK export glue; create `crates/sdk/tests/sdk_tests/ptc.rs` and register it in `crates/sdk/tests/sdk_tests.rs`. Verify the complete per-run tool/extension set is used, summary-only model history, preserved child audit events, separate session/run authority and unchanged plain `.bun()` behavior.
- [ ] **Integrate interactive and headless hosts using the same binding.** Modify `crates/cli/src/runtime/interactive.rs` and `crates/cli/src/headless.rs`; add focused integration tests in existing CLI test modules. Verify PlanGate, human/headless/automatic approval, selected-tool filters, plugin hook order, MCP selection denial and model/catalog rebuilds. Retain raw cached Bun identity in `runtime/local_tools.rs`; verify reset releases old bindings. Do not enable PTC in worker factories in this patch.
- [ ] **Document the actual supported contract.** Modify the PTC wrapper's tool description, `crates/cli/src/main.rs` guidance, and create `docs/architecture/programmatic-tool-calling.md`. Verify examples use actual tool schemas and state outer-serial scheduling, batch completion, transformed results, native-code trust and no rollback; do not advertise capability for unbound/plain runtimes.
- [ ] **Run regression checks and review the final footprint.** No new production files in this step. Run formatter, focused tools/SDK/CLI tests, unchanged core concurrency/extension/agent-loop tests, then workspace tests and clippy. Compare ordinary dispatch/Bun performance with baseline using existing benchmark infrastructure where applicable; inspect final diff for zero core production edits, no duplicated policy/scheduling, no unexpected dependencies, and no unrelated changes. Record results and remaining platform limitations in this plan.

## Validation commands (implementation phase only)

- `cargo fmt --all -- --check`
- `cargo test -p orca-harness-tools`
- `cargo test -p orca-harness-core`
- Run SDK and CLI test packages using the exact package names from their manifests.
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`

Run real Bun integration tests in an environment with Bun installed; a skipped runtime test is not verification of IPC. Use existing burst/concurrency tests to assert active counts and ordering rather than relying only on wall-clock thresholds. Before invoking benchmarks, follow the repository's benchmark audit skill and preserve the baseline.

## Blast radius and explicit trade-offs

Core execution algorithms, model providers, individual tool implementations, Python, MCP transports and DAG/workflow remain unchanged. The intended production footprint is concentrated in two new tools modules, small Bun lifecycle integration, and the actual SDK/CLI assembly points; host assembly may account for more edits than the bridge because core registries are private and policies must not be duplicated.

The principal behavioral addition is an explicitly enabled outer-serial Bun wrapper with parallel child batches. Reuse also means accepting current transformed-result and batch-completion semantics. This is smaller and less invasive than generalized nested scheduling, but its safety must be demonstrated with real host hooks before rollout; a promise of zero core edits must never justify a policy bypass.

## Planning status

Read-only repository inspection completed; no implementation or tests executed for this plan. Worktree status/diff and runtime experiments were not available under the current plan-mode restrictions and must be checked at the first implementation step. The scheduling composition is a proposed test-gated implementation, not a verified capability. The available `mini-plan` and `dry-yagni` skills informed this plan; `disciplined-code-quality` was not present in the listed skill catalog.
