# Explore Orca Harness

**Goal:** Build a working mental model of the app — kernel, tools, extensions, CLI — and end with a short written map of how the pieces fit.

**Approach:** Walk the workspace in dependency order (core → adapters → tools → extensions → CLI), reading the key files of each layer, then run the tests and an example to see the whole thing actually work. Everything is read-only except the final write-up.

**Files touched:** only this plan file is updated (checkboxes + findings appended).

## Steps

- [x] **Kernel first** — read `crates/harness-core/src`: the `Agent`, `Loop`, `Dispatcher`, `Tool`, `Extension` types and `Limits`. Verified: `Agent.run` → `agent_loop.run` → on a `ToolCalls` response, `Dispatcher.execute` → each tool's `call` (fan-out, concurrency-classified). Trait names: `Model`, `Tool`, `Extension`, `DeltaSink`.
- [x] **Model adapters** — skimmed `crates/model-providers` (OpenAI-compatible chat completions, OpenRouter catalog/presets, and Codex Responses). `Model::generate` / `generate_streaming(deltas when an extension listens)`.
- [x] **Tools** — listed `crates/tools/src`: shell, process, pykernel, subagent, read/write/edit_file, list_dir, grep, glob, todo_write (+ fs_admin, tool-extensions::{web,mcp,skills}). A tool registers via `Agent::tool(…)`; contract is `schema()` + `concurrency(input)` + `call(input, ctx) → Value`.
- [x] **Extensions** — read `crates/extensions/src`: event stream, tool policy, truncation(+store/read_tool_result), retry, usage, session record/resume/rewind/fork. Each subscribes only to the `Subscriptions` hooks it needs.
- [x] **CLI** — traced one turn in `crates/cli/src`: input → model → tool dispatch → approval (before_tool) → render; plan mode is an allowlist `PlanGate` (`mode.rs`/`plan.rs`) with `docs/plan/` as the only writable area.
- [x] **Run it** — `cargo test --workspace` passes (all suites green); `cargo run --example fake_llm` shows 4×100 ms tools finishing in ~102 ms (peak concurrency 4).
- [x] **Write the map** — appended below.

## How the pieces fit

**What this is:** Orca Harness is a minimal, embedded **agent execution kernel** in Rust plus `orcacode`, a terminal host built on it. The kernel is a single primitive: *the loop is sacred, everything around it is extensible.*

```
         orcacode (host / terminal UI)
              │ composes
   ┌──────────┴──────────────────────┐
   │  harness-core (the kernel)      │
   │  Agent → Loop → Dispatcher → Tool│
   │        ↕ (Extension hooks)       │
   └─────────────────────────────────┘
   model-providers                    (Model adapters)
   tools / tool-extensions                    (Tools)
   extensions                        (canonical extensions)
```

### Layers, bottom-up

1. **Kernel (`harness-core`)** — owns the whole hot path and nothing the host should change:
   - `Agent` — configuration + entry point; holds `Model`, `ToolRegistry`, `ExtensionRegistry`, `Limits`, one `Dispatcher`.
   - `Context` — model-visible conversation (the transcript), *not* durable memory. `Message::System | User | Assistant | Tool`.
   - `Loop` (`agent_loop.rs`) — deliberately boring: check cancellation → deadline → step limit, run before/after model hooks, generate, dispatch, repeat; stops on a `Final` text response.
   - `Dispatcher` — the correctness-critical part. Resolves tools, preserves call→result identity, classifies concurrency, fans parallel-safe calls out concurrently, serializes conflicting ones, applies max-parallelism, propagates cancellation/deadlines, normalizes failures, restores deterministic model-visible order.
   - `Tool` — the only way an agent acts on the world: a JSON schema, a *concurrency classifier* (`Parallel` / `Serial` / `Keyed(key)` / `Keys`), and an async `call(input, ctx) → Value`.
   - `Extension` — the single extensibility seam. Subscribes to lifecycle events (`on_agent_start` … `on_agent_end`); unsubscribed events cost one empty-slice check. Key hooks: `before_tool` (returns `Continue / Rewrite / Deny`), `around_tool` (wrap/retry, `Next` is Copy so it can run the continuation more than once), `after_tool` (truncate/redact/normalize).
   - `Limits` — mechanisms (`max_steps`, `max_parallel_tools`, `deadline`), not policy; the host decides values.
   - `Testing` — `ScriptedModel`, a fake LLM that replays scripted responses.

2. **Model adapters** — behind the `Model` trait so the loop has zero provider logic. `model-providers` keeps providers modular behind the core `Model` trait: OpenAI-compatible chat completions normalize token accounting and streaming, OpenRouter reuses that protocol with catalog and attribution support, and Codex owns its separate Responses protocol.

3. **Tools (`crates/tools`)** — ordinary `Tool` impls, nothing privileged: `shell` (local or via `Executor::ssh`/`docker`), `process` (persistent sessions / background), `pykernel` (persistent Python), `subagent` (in-process fan-out, shared depth cap), file trio `read/write/edit_file` (read-before-write guarded), `list_dir`, `grep`, `glob`, `todo_write` (host-visible plan), plus an opt-in `fs_admin` bundle. `tool-extensions` keeps opt-in modules for SSRF-guarded web access, stdio MCP servers, and `SKILL.md` discovery. All register via `Agent::tool(…)`; schemas list in registration order.

4. **Extensions (`crates/extensions`)** — one file per capability, each subscribing only to the hooks it needs:
   - `EventStream` — typed lifecycle events to a sink / NDJSON / TUI;
   - `ToolPolicy` — allowlist/denylist/rule in `before_tool`;
   - `Truncation` + `TruncationStore` — cap oversized outputs in `after_tool`, keep the full original for `read_tool_result`;
   - `ToolRetry` / `RetryModel` — retry failing tools (`around_tool` wrapper) and transient model errors (`Model` decorator);
   - `UsageMeter` — accumulate token usage;
   - `SessionHandler` — record the transcript to append-only JSONL; resume, rewind, fork.

5. **CLI (`orcacode`)** — the composition root wiring the kernel, tools, model adapters, extensions, and a terminal UI. One turn: terminal read → model call (streaming deltas rendered live) → if the model returns tool calls, the Dispatcher runs them concurrently → each gated tool (`shell`/`write_file`/`edit_file`/`web_fetch`/`pykernel`/`subagent`) pauses on an `Approval` before_tool extension (y/a/A/n) → results feed back → loop repeats → final text rendered.

### Plan mode

An *allowlist*, not a denylist: a `PlanGate` before_tool extension denies every tool not in `READ_ONLY_TOOLS`; anything unknown (MCP, future tools) fails closed. The one writable place is `docs/plan/`. `PlanGate` reads a shared atomic `ModeHandle` on every call, so flipping `/mode` applies to the *very next* tool call — no agent rebuild. The gate is registered before `Approval`, so a denied write never reaches the human as a prompt, and an always-allow grant cannot widen past the plan directory. Denied tools in plan mode get a reason pointing the model back to investigation tools.

### Concurrency headline

Concurrently-safe tool calls fan out instantly, converting `sum(latencies)` into `max(latency)`. `fake_llm` shows 4 sources at 100 ms finishing in ~102 ms (peak concurrency 4). The kernel targets <1 ms added latency for 100 parallel no-op calls; real `write_file`/`read_file`/`shell` probes confirm it (64 20 ms shell subprocesses = **19.5×** speedup over serial).

### 3 things that surprised me

1. `Context` is model-visible state and explicitly *not* the durable memory; durability is pushed all the way out to an Extension (`SessionHandler`).
2. Tool concurrency is declared per-call via a classifier (`Keyed("pykernel")`, keyed `todo_write`, etc.), so the dispatcher never needs to know what a tool does — correctness comes from the tool telling the kernel how it may schedule itself.
3. `/mode` does *not* rebuild the agent: `PlanGate` reads a shared atomic handle per call so the new mode applies instantly, and `mode.rs` explicitly warns "do not fix this into a rebuild".