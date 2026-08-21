# Orca Harness

A minimal, high-performance agent execution kernel in Rust. It keeps the
privileged path tiny and adds capabilities through Extensions.

> **The loop is sacred. Everything around it is extensible.**

Orca Harness is an execution primitive, not the whole Orca platform. It is a
standalone Cargo workspace inside the monorepo: nothing in the Go control
plane or the Node sidecars depends on it, and it depends on nothing here.

## Boundary

The Harness owns: agent configuration, context, model invocation, the loop,
tool dispatch, concurrent execution, cancellation, deadlines, limits,
call/result pairing, deterministic ordering, and the Extension lifecycle.

The Orca platform owns: distributed scheduling, durable sessions,
microVM lifecycle, networking, tenancy, fleet management, and control-plane
APIs.

```text
              ORCA PLATFORM
┌─────────────────────────────────┐
│ scheduling  sessions  isolation │
│ persistence networking scaling  │
└───────────────┬─────────────────┘
                ▼
┌─────────────────────────────────┐
│          ORCA HARNESS           │
│ Agent → Loop → Dispatcher → Tool│
│           ↕                     │
│       Extensions                │
└─────────────────────────────────┘
```

## Layout

```text
orca-harness/
├── crates/
│   ├── harness-core/     # the kernel: Agent, Context, Model, Loop,
│   │                     # Dispatcher, Tool, Extension, Limits, errors
│   ├── model-openai/     # OpenAI-compatible chat-completions adapter
│   ├── tools/            # core host/target tools: shell, process
│   │                     # (persistent sessions / background processes),
│   │                     # pykernel (persistent Python compute), subagent
│   │                     # (in-process agent fan-out with adjustable
│   │                     # nesting), read/write/edit/list files, grep,
│   │                     # glob — the set that makes an agent
│   │                     # independently useful; plus an opt-in fs-admin
│   │                     # bundle (copy/rename/delete/mkdir/stat)
│   ├── tools-web/        # opt-in web tools: web_fetch (HTML→markdown,
│   │                     # SSRF-guarded URL policy), plus web_search and
│   │                     # web_crawl backed by Firecrawl (provider trait
│   │                     # for swapping the search backend)
│   ├── extensions/       # critical extensions: event stream, tool
│   │                     # policy, truncation (+ store paired with the
│   │                     # read_tool_result tool), retry, usage metering,
│   │                     # session recording/resume (JSONL transcripts,
│   │                     # --continue / --resume / /sessions)
│   └── cli/              # `orcacode`: interactive terminal host (streaming
│                         # REPL, tool approvals, headless mode)
```

## Try it

`orcacode` is a reference host proving the harness drives a real terminal
agent. It talks to any OpenAI-compatible endpoint; with no key set it
defaults to a local Ollama server:

```bash
cargo run --release -p orcacode                      # interactive REPL
cargo run --release -p orcacode -- -p "count the rust files"   # headless
cargo run --release -p orcacode -- --json -p "..."   # NDJSON event stream
```

Interactive mode marks each user turn with a strong transcript spine and
groups live reasoning and parallel tool calls into a per-turn activity rail.
Completed work collapses to a compact summary; `ctrl+o` expands its full tree
in place, while `/expand` prints the raw output of an individual tool call.
Tool rows use `□`, `✓`, and `×` for running, successful, and failed states.
Failed output expands inline while work is live. Running subagents show their
inner tool calls as an indented nested rail (collapsed into the expandable
record when they finish), and the status line counts live background work
(`procs 2 · pykernel · agents 3`). Gated tools (`shell`, `write_file`,
`edit_file`, `pykernel`, `subagent`) pause behind an approval prompt:
`y` allows once, `a` always allows for the session, `A` always allows and
saves the grant to the config file scoped to this workspace (future
sessions in the same directory skip the prompt; other directories still
ask), and `n` denies. Saved grants are listed and revocable
from `/settings`. Prompts submitted during a run wait in a FIFO rail
above the activity indicator and start automatically in submission order;
`/queue clear` discards the waiting prompts. Esc cancels in-flight runs
(killing spawned subprocesses) and pauses the queue, and the conversation
persists across turns. Sessions are recorded per workspace under
`~/.config/orcacode/sessions/` as append-only JSONL; `--continue` resumes
the latest, `--resume <id>` a specific one, `/sessions` opens a picker to
resume one from the TUI, and `--no-session` opts out. `/clear` starts a
fresh session and stops all background work (processes, pykernel,
subagents). `ORCA_MODEL`,
`ORCA_BASE_URL`, and
`OPENAI_API_KEY` (or `--model`, `--base-url`, `--api-key`) select the
endpoint. API keys entered in the TUI, the active provider, the theme, and
the last model picked per provider persist to
`~/.config/orcacode/config.json` (owner-only permissions; `$ORCA_CONFIG_DIR`
overrides the directory) and are reused on later runs — flags and
environment variables always win over the saved values. `/settings` shows
the current values and jumps into the provider, model, theme, and api-key
pickers. `/extensions` opens a picker over the optional harness extensions —
output truncation (default on) and tool retry (default off) — where
enter toggles the selected one (`/extensions enable|disable <name>`
works directly); toggles persist to the same config file and apply to
interactive and headless runs alike.

## Core tools

`orca-harness-tools` gives an agent the ability to act on a machine and
hand results to the model:

- `shell` — run a command, capture stdout/stderr/exit code. Runs on the
  host by default; point it at another machine or container with an
  `Executor` (`Executor::ssh("user@host")`, `Executor::docker_exec("ctr")`)
  and the model drives that target through the same contract. Kills the
  child on cancellation, caps output, enforces a timeout.
- `read_file`, `write_file`, `edit_file`, `list_dir`, `grep` — all rooted
  at a `Workspace` that rejects absolute paths and `..` escapes. Writes and
  edits are `Keyed` by path, so same-file writes serialize while
  different-file writes run concurrently.

`core_tools(&ws)` returns the recommended default set ready to register.

## Critical extensions

`orca-harness-extensions` — each subscribes only to the hooks it uses, so
registering an unused one costs nothing on the hot path:

- **EventStream** — turns the lifecycle into a typed `HarnessEvent` stream
  delivered to a sink (closure or channel). Tags mirror a platform NDJSON
  union (`assistant_delta`, `reasoning_delta`, `assistant`, `tool_call`,
  `tool_result`, `usage`, `result`, `error`) so a host can serialize them
  directly. This is the main seam for building on the harness.
- **ToolPolicy** — allow/deny tool calls before execution (allowlist,
  denylist, or a custom predicate).
- **Truncation** — cap oversized tool outputs to protect the context
  window and any downstream line cap.
- **ToolRetry** / **RetryModel** — retry failing tools (an `around_tool`
  extension) and transient model errors (a `Model` decorator). The CLI
  wires `ToolRetry` to also retry the failures core tools report *as
  data* — a nonzero shell exit, an HTTP 5xx from `web_fetch` — and
  mirrors the same policy inside subagents, so every level of the agent
  tree retries.
- **UsageMeter** — accumulate self-reported token usage across a run,
  readable via a shared handle after it returns.

## Quick start

```rust
use orca_harness_core::{Agent, FnTool};
use orca_harness_model_openai::OpenAiModel;
use serde_json::json;

let model = OpenAiModel::new("gpt-4o").api_key(std::env::var("OPENAI_API_KEY")?);
let shell = FnTool::new("shell", "Run a command", json!({"type": "object"}),
    |input, _ctx| async move { Ok(input) });

let agent = Agent::new(model).tool(shell);
let result = agent.run("Fix the failing test").await?;
```

Models can stream: `Model::generate_streaming` emits incremental
`ModelDelta`s (assistant text and reasoning fragments) to subscribed
extensions while the authoritative `ModelResponse` is still the only thing
the loop acts on. Adapters that don't stream need no changes — the default
implementation falls back to `generate`, and the loop only takes the
streaming path when an extension actually subscribes to deltas.

Concurrent tool execution, cancellation, deadlines, step limits, and
deterministic call/result ordering are built into the kernel. Everything
else (memory, MCP, permissions, tracing, retries, sandbox routing, ...)
composes through the `Extension` trait — with subscriptions compiled into
per-event arrays at construction, so unused extensibility approaches zero
cost.

## Concurrency semantics

A tool classifies each call via `Tool::concurrency(&input)`:

- `Parallel` (default) — safe to run alongside anything.
- `Serial` — exclusive: nothing else executes while it does.
- `Keyed(key)` — calls sharing a key serialize in call order; unrelated
  calls continue concurrently (e.g. key writes by target path).

`Limits::max_parallel_tools` caps simultaneous execution. Whatever order
tools *finish* in, the model always sees results in the original call order
with call ids paired.

## Performance

The metric that matters is not binary startup: it is the overhead added
between a model emitting tool calls and those tools doing useful work.
The kernel's goal: **< 1 ms added latency to fan out 100 parallel tool
calls, p99 dispatch overhead < 2 ms** — so even a 0.1 ms filesystem tool
barely notices the harness.

`examples/fanout_probe.rs` measures it directly with no-op tools, where
T0 = dispatch entry, T1 = first tool body started, T2 = last tool body
started:

```bash
cargo run --release --example fanout_probe -- 100 300   # batch size, iterations
```

Measured on a 4-core Linux box (release build, tokio multi-thread):

| Metric                        |    p50 |    p99 | target        |
| ----------------------------- | -----: | -----: | ------------- |
| 1 call, dispatch (T1−T0)      | 0.4 µs | 0.6 µs | < 10 µs       |
| 10 calls, fan-out (T2−T0)     |  60 µs | 121 µs | < 100 µs-ish  |
| 100 calls, fan-out (T2−T0)    | 148 µs | 288 µs | < 1 ms        |
| 100 calls, full round-trip    | 183 µs | 326 µs | < 1 ms        |

How the hot path stays that flat:

- **No cross-thread handoff for a single call**: one unit of every batch
  runs inline in the dispatching task after the rest are spawned, so a
  1-call batch is pure function-call overhead (~1 µs round-trip).
- **Synchronization is elided when it cannot constrain the batch**: the
  parallelism semaphore only exists when the batch exceeds
  `max_parallel_tools`; the Serial-exclusivity RwLock only exists when
  the batch contains a `Serial` call.
- **No per-call `ToolCall` clones**: tasks address the batch through one
  shared `Arc<[ToolCall]>`, and each call's input `Value` is moved, not
  copied, into execution.
- Extension hooks compile to per-event arrays; with no subscribers each
  hook site is an empty-slice check.

The residual T1−T0 for large batches (~40 µs here) is tokio waking a
parked worker thread; on a busy server with hot workers it shrinks
further. Numbers scale with core count — re-measure on your target
hardware with the probe.

### Real core-tool fan-out

`fanout_probe` above measures pure dispatch with no-op tools. The
`orca-harness-tools` probe drives the **actual** `write_file`,
`read_file`, and `shell` tools through the real Dispatcher — real
filesystem writes and real subprocesses — so the harness overhead is
measured against genuine tool latency:

```bash
cargo run -p orca-harness-tools --release --example tool_fanout_perf
```

Measured on the same 4-core box (batch of 100, or 64 for subprocesses):

| Case (one model turn)          | wall p50 | throughput | speedup vs serial |
| ------------------------------ | -------: | ---------: | ----------------- |
| 100 × `write_file`, distinct   |   3.1 ms | 32k /sec   | —                 |
| 100 × `read_file`, same file   |   1.4 ms | 72k /sec   | —                 |
| 64 × `shell`, 20 ms subprocess |  66 ms   | ~1k /sec   | **19.5×**         |

The shell case is the headline: 64 subprocesses that would take 1.28 s
run serially finish in 66 ms concurrently. Its ~38 ms fan-out overhead is
the OS cost of `fork`/`exec`-ing 64 processes on 4 cores (spawned
concurrently), not harness scheduling — the kernel's own dispatch
overhead stays in the sub-millisecond range the no-op probe shows. For
latency-bound tools (HTTP, remote MCP, subprocesses) the harness turns
`sum(latencies)` into `max(latency)`, which is the whole point of
building concurrency into the kernel.

## Develop

```bash
cd orca-harness
cargo test --workspace        # unit + integration tests (fake scripted LLM)
cargo run --example fake_llm  # end-to-end run showing 4-way tool fan-out
cargo run -p orca-harness-tools --example agent_with_tools
                              # full stack: kernel + host tools + extensions
cargo bench                   # criterion suite: dispatch latency, fan-out,
                              # extension overhead, keyed scheduling
cargo clippy --workspace --all-targets
```

From the repo root: `make harness-test` / `make harness-bench`.

The integration tests drive the kernel end-to-end through
`testing::ScriptedModel`, a fake LLM that replays scripted responses, and
assert the concurrency mechanism directly: barrier tests prove genuine
overlap, probes assert per-key serialization and the parallelism
high-water mark, and cancellation/deadline tests prove propagation into
in-flight fan-out.

## v0.1 scope

Kernel (`harness-core`): Agent, Context, Model, Loop, Dispatcher, Tool,
ToolRegistry, Extension, ExtensionRegistry, Limits, cancellation, typed
errors — plus the OpenAI-compatible adapter and a basic benchmark suite.

Deliberately not in the kernel: durable sessions, workflow DAGs, queues,
planners, distributed scheduling, built-in memory, UI. Those belong to the
platform, to Extensions, or to hosts like the `orcacode` CLI.
