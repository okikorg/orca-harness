# Orca Harness

A minimal, high-performance agent execution kernel in Rust. It keeps the
privileged path tiny and adds capabilities through Extensions.

> **The loop is sacred. Everything around it is extensible.**

Orca Harness is an execution primitive, not the whole Orca platform. It is a
standalone Cargo workspace inside the monorepo: nothing in the Go control
plane or the Node sidecars depends on it, and it depends on nothing here.

## Boundary

| Owner                      | Responsibilities                                                                                                                                                                                         |
| :------------------------- | :------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **The Harness owns**       | agent configuration, context, model invocation, the loop, tool dispatch, concurrent execution, cancellation, deadlines, limits, call/result pairing, deterministic ordering, and the Extension lifecycle |
| **The Orca platform owns** | distributed scheduling, durable sessions, microVM lifecycle, networking, tenancy, fleet management, and control-plane APIs                                                                               |

```text
            ORCA PLATFORM
┌────────────────────────────────────┐
│  scheduling  sessions  isolation   │
│  persistence  networking  scaling  │
└─────────────────┬──────────────────┘
                  ▼
┌────────────────────────────────────┐
│            ORCA HARNESS            │
│  Agent → Loop → Dispatcher → Tool  │
│                 ↕                  │
│             Extensions             │
└────────────────────────────────────┘
```

## Layout

```text
orca-harness/
├── crates/
│   ├── harness-core/ # the kernel: Agent, Context, Model, Loop, Dispatcher, Tool, Extension,
│   │                 # Limits, errors
│   ├── model-openai/ # OpenAI-compatible chat-completions adapter
│   ├── tools/        # core host/target tools: shell, process (persistent sessions / background
│   │                 # processes), pykernel (persistent Python compute), subagent (in-process
│   │                 # agent fan-out with adjustable nesting), read/write/edit/list files,
│   │                 # grep, glob — the set that makes an agent independently useful; plus an
│   │                 # opt-in fs-admin bundle (copy/rename/delete/mkdir/stat)
│   ├── tools-web/    # opt-in web tools: web_fetch (HTML→markdown, SSRF-guarded URL policy),
│   │                 # plus web_search and web_crawl backed by Firecrawl (provider trait for
│   │                 # swapping the search backend)
│   ├── tools-mcp/    # opt-in MCP client: connect stdio MCP servers (/mcp add <name> <command>
│   │                 # in the CLI) and expose their tools as mcp__<server>__<tool>
│   ├── extensions/   # critical extensions: event stream, tool policy, truncation (+ store
│   │                 # paired with the read_tool_result tool), retry, usage metering, session
│   │                 # recording/resume (JSONL transcripts, --continue / --resume / /sessions)
│   └── cli/          # `orcacode`: interactive terminal host (streaming REPL, tool approvals,
│                     # headless mode)
```

## Try it

`orcacode` is a reference host proving the harness drives a real terminal
agent. It talks to any OpenAI-compatible endpoint; with no key set it
defaults to a local Ollama server:

```bash
cargo run --release -p orcacode                    # interactive REPL
cargo run --release -p orcacode -- -p "..."        # headless: single prompt
cargo run --release -p orcacode -- --json -p "..." # headless: NDJSON events
```

### Interactive mode

Each user turn gets a strong transcript spine; live reasoning and parallel
tool calls group into a per-turn activity rail. Completed work collapses to
a compact summary:

- `ctrl+o` expands a turn's full output tree in place; `/expand <id>` prints
  one tool call's raw output.
- Tool rows use `□`, `✓`, and `×` for running, successful, and failed;
  failed output expands inline while work is live.
- Subagents render their inner tool calls as an indented nested rail,
  collapsed into the expandable record when they finish. The status line
  counts live background work (`procs 2 · pykernel · agents 3`).

**Tool approvals** — gated tools (`shell`, `write_file`, `edit_file`,
`pykernel`, `subagent`) pause behind a prompt:

| Key | Effect                                                                           |
| :-- | :------------------------------------------------------------------------------- |
| `y` | allow once                                                                       |
| `a` | always allow for this session                                                    |
| `A` | always allow **and** save the grant to the config file, scoped to this workspace |
| `n` | deny                                                                             |

Saved grants are scoped to the workspace directory — future sessions in the same directory
skip the prompt, other directories still ask — and are listed and revocable from `/settings`.

**Queuing and cancellation** — prompts submitted during a run wait in a FIFO
rail above the activity indicator and start automatically in submission
order (`/queue clear` discards them). Esc cancels the in-flight run (killing
spawned subprocesses) and pauses the queue; the conversation persists across
turns.

**Sessions** — recorded per workspace under `~/.config/orcacode/sessions/`
as append-only JSONL:

| Command / flag  | Effect                                                                                                     |
| :-------------- | :--------------------------------------------------------------------------------------------------------- |
| `--continue`    | resume the latest session                                                                                  |
| `--resume <id>` | resume a specific session                                                                                  |
| `/sessions`     | open a picker and resume from the TUI                                                                      |
| `--no-session`  | opt out of recording                                                                                       |
| `/clear`        | empty the current session in place (same id) and stop all background work (processes, pykernel, subagents) |

**Endpoint selection** — `ORCA_MODEL`, `ORCA_BASE_URL`, and
`OPENAI_API_KEY` (or `--model`, `--base-url`, `--api-key`) choose the
provider. Keys entered in the TUI, the active provider, the theme, and the
last model picked per provider persist to
`~/.config/orcacode/config.json` (owner-only permissions; `$ORCA_CONFIG_DIR`
overrides the directory) and are reused on later runs — flags and
environment variables always win over saved values. `/settings` shows the
current values and jumps into the provider, model, theme, and api-key
pickers.

**Extensions** — `/extensions` opens a picker over the optional harness
extensions — output truncation (default on) and tool retry (default off);
enter toggles the selected one, and `/extensions enable\|disable <name>`
works directly. Toggles persist to the same config file and apply to
interactive and headless runs alike.

**MCP servers** — `/mcp` opens a picker over the configured stdio MCP
servers; space (or enter) toggles one on or off, and the row shows its
live tool count:

| Command                        | Effect                                            |
| :----------------------------- | :------------------------------------------------ |
| `/mcp`                         | picker — space toggles the selected server        |
| `/mcp add <name> <command>`    | save a server and connect it                      |
| `/mcp remove <name>`           | forget a server and drop its tools                |

A server's tools are exposed to the model as `mcp__<server>__<tool>`, so
`<name>` must be letters, digits, `-`, or `_`. Toggling is cheap: reloads
diff the config against the live connections, so flipping one server
leaves the others' processes untouched. A server that fails to connect
reports why and is skipped — it never blocks the rest.

Servers persist to the config file, either as a bare command string or as
`{"command": …, "enabled": false}` so a disabled server keeps its command:

```json
"mcp": {
  "fetch": "uvx mcp-server-fetch",
  "docs": { "command": "npx -y mcp-remote https://…", "enabled": false }
}
```

The command is split on whitespace and run without a shell — there is no
quoting, no variable expansion, and no per-server environment, so a server
needing a credential reads it from the environment the CLI was launched
with. The picker redacts credentials it can recognize (header values,
`--api-key`/`--token` arguments, URL userinfo and query secrets, bare
token-shaped words) so a pasted key is not left on screen; `${VAR}`
references are shown as written, since they name a variable rather than
carry one.

Remote HTTP/SSE servers are reachable through a stdio bridge:
`/mcp add docs npx -y mcp-remote https://mcp.example.com/mcp`.

## Core tools

`orca-harness-tools` gives an agent the ability to act on a machine and hand
results to the model:

- `shell` — run a command, capture stdout/stderr/exit code. Runs on the host
  by default; point it at another machine or container with an `Executor`
  (`Executor::ssh("user@host")`, `Executor::docker_exec("ctr")`) and the
  model drives that target through the same contract. Kills the child on
  cancellation, caps output, enforces a timeout.
- `read_file`, `write_file`, `edit_file`, `list_dir`, `grep`, `glob` — all
  rooted at a `Workspace` that rejects absolute paths and `..` escapes.
  Writes and edits are `Keyed` by path: same-file writes serialize while
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
- **Truncation** — cap oversized tool outputs to protect the context window
  and any downstream line cap. Paired with the harness's `read_tool_result`
  tool, so the model can page back through the full untruncated output.
- **ToolRetry** / **RetryModel** — retry failing tools (an `around_tool`
  extension) and transient model errors (a `Model` decorator). The CLI wires
  `ToolRetry` to also retry failures core tools report *as data* — a nonzero
  shell exit, an HTTP 5xx from `web_fetch` — and mirrors the same policy
  inside subagents, so every level of the agent tree retries.
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

| Class                | Behavior                                                                                                              |
| :------------------- | :-------------------------------------------------------------------------------------------------------------------- |
| `Parallel` (default) | safe to run alongside anything                                                                                        |
| `Serial`             | exclusive — nothing else executes while it does                                                                       |
| `Keyed(key)`         | calls sharing a key serialize in call order; unrelated calls continue concurrently (e.g. writes keyed by target path) |

`Limits::max_parallel_tools` can cap simultaneous execution; the default is
unbounded, and tools with a narrower useful width regulate themselves (the
file tools share a bounded filesystem I/O gate). Whatever order tools
*finish* in, the model always sees results in the original call order with
call ids paired.

## Performance

The metric that matters is not binary startup: it is the overhead added
between a model emitting tool calls and those tools doing useful work. The
kernel's goal: **< 1 ms added latency to fan out 100 parallel tool calls,
p99 dispatch overhead < 2 ms** — so even a 0.1 ms filesystem tool barely
notices the harness.

`examples/fanout_probe.rs` measures it directly with no-op tools, where
T0 = dispatch entry, T1 = first tool body started, T2 = last tool body
started:

```bash
cargo run --release --example fanout_probe -- 100 300   # batch size, iterations
```

Measured on a 4-core Linux box (release build, tokio multi-thread):

| Metric                     |    p50 |    p99 |    target |
| :------------------------- | -----: | -----: | --------: |
| 1 call, dispatch (T1−T0)   | 0.4 µs | 0.6 µs |   < 10 µs |
| 10 calls, fan-out (T2−T0)  |  60 µs | 121 µs | < ~100 µs |
| 100 calls, fan-out (T2−T0) | 148 µs | 288 µs |    < 1 ms |
| 100 calls, full round-trip | 183 µs | 326 µs |    < 1 ms |

How the hot path stays that flat:

- **No cross-thread handoff for a single call** — one unit of every batch
  runs inline in the dispatching task after the rest are spawned, so a
  1-call batch is pure function-call overhead (~1 µs round-trip).
- **Synchronization is elided when it cannot constrain the batch** — the
  parallelism semaphore only exists when the batch exceeds
  `max_parallel_tools`; the Serial-exclusivity RwLock only exists when the
  batch contains a `Serial` call.
- **No per-call `ToolCall` clones** — tasks address the batch through one
  shared `Arc<[ToolCall]>`, and each call's input `Value` is moved, not
  copied, into execution.
- Extension hooks compile to per-event arrays; with no subscribers each
  hook site is an empty-slice check.

The residual T1−T0 for large batches (~40 µs here) is tokio waking a parked
worker thread; on a busy server with hot workers it shrinks further. Numbers
scale with core count — re-measure on your target hardware with the probe.

### Real core-tool fan-out

`fanout_probe` measures pure dispatch with no-op tools. The
`orca-harness-tools` probe drives the **actual** `write_file`, `read_file`,
and `shell` tools through the real Dispatcher — real filesystem writes and
real subprocesses — so harness overhead is measured against genuine tool
latency:

```bash
cargo run -p orca-harness-tools --release --example tool_fanout_perf
```

Measured on the same 4-core box (batch of 100, or 64 for subprocesses):

| Case (one model turn)          | wall p50 | throughput | speedup vs serial |
| :----------------------------- | -------: | ---------: | ----------------: |
| 100 × `write_file`, distinct   |   3.1 ms |    32k/sec |                 — |
| 100 × `read_file`, same file   |   1.4 ms |    72k/sec |                 — |
| 64 × `shell`, 20 ms subprocess |    66 ms |    ~1k/sec |         **19.5×** |

The shell case is the headline: 64 subprocesses that would take 1.28 s
serially finish in 66 ms concurrently. Its ~38 ms fan-out overhead is the OS
cost of `fork`/`exec`-ing 64 processes on 4 cores (spawned concurrently),
not harness scheduling — the kernel's own dispatch overhead stays in the
sub-millisecond range the no-op probe shows. For latency-bound tools (HTTP,
remote MCP, subprocesses) the harness turns `sum(latencies)` into
`max(latency)`, which is the whole point of building concurrency into the
kernel.

### Binary size and footprint

`orcacode` ships as a single static binary — no bundled runtime, no wrapper
processes:

| Metric                                                              |      Value |
| :------------------------------------------------------------------ | ---------: |
| Release binary (default profile)                                    |    11.7 MB |
| Release binary (`lto = "fat"`, `codegen-units = 1`, `strip = true`) | **7.2 MB** |
| Idle resident memory (one live session)                             |      ~8 MB |
| Processes at runtime                                                |          1 |

### Compare to

Idle-state measurements taken on the same machine (Apple Silicon Mac,
2026-08-22) from the installed binaries and live processes — not vendor
claims. Every row is a live session doing nothing, sampled the same way
(`ps` RSS over the CLI's own process tree after ~10 s idle):

| CLI (version)                                     |              Binary | Idle RSS (live session) | Processes |
| :------------------------------------------------ | ------------------: | ----------------------: | --------: |
| `orcacode` 0.1.0 (this repo)                      |          **7.2 MB** |               **~8 MB** |         1 |
| `fx` 0.0.5 (for scale)                            |              6.4 MB |                  ~21 MB |         1 |
| `pi` 0.84.2 (`@earendil-works/pi-coding-agent`)   |      131 MB install |                 ~211 MB |  1 + node |
| Codex 0.149.0 (`@openai/codex`)                   | 210 MB + 55 MB host |                 ~340 MB |   up to 3 |
| `prime-agent` 0.7.4                               |      265 MB install |                 ~400 MB | 1 + node + py |
| `omp` 17.4.2 (`@oh-my-pi/pi-coding-agent`)        |  233 MB + 63 MB bun |                 ~406 MB |   1 + bun |
| Claude Code 2.1.220 (`@anthropic-ai/claude-code`) |              245 MB |                 ~456 MB |         1 |

Numbers are the core CLI only: any MCP servers configured for a CLI spawn
on top of this at startup (on the measurement machine they added 1–2 GB and
up to a dozen node processes to Codex, omp, and Claude Code alike).

Codex, Claude Code, and omp bundle a JavaScript runtime (Bun/Node); pi runs
on a Node process — hence the order-of-magnitude gaps on both axes. A
pure-Rust kernel sits next to `fx`, not next to the JS-bundled agents, which
is what makes it cheap to embed as a platform's execution primitive
(`ORCA_HARNESS_BIN`) and to run many agents per host.

## Develop

```bash
cd orca-harness
cargo test --workspace   # unit + integration tests (fake scripted LLM)
cargo run --example fake_llm   # end-to-end run showing 4-way tool fan-out
cargo run -p orca-harness-tools --example agent_with_tools
                         # full stack: kernel + host tools + extensions
cargo bench              # criterion suite: dispatch latency, fan-out,
                         # extension overhead, keyed scheduling
cargo clippy --workspace --all-targets
```

From the repo root: `make harness-test` / `make harness-bench`.

The integration tests drive the kernel end-to-end through
`testing::ScriptedModel`, a fake LLM that replays scripted responses, and
assert the concurrency mechanism directly: barrier tests prove genuine
overlap, probes assert per-key serialization and the parallelism high-water
mark, and cancellation/deadline tests prove propagation into in-flight
fan-out.

## v0.1 scope

Kernel (`harness-core`): Agent, Context, Model, Loop, Dispatcher, Tool,
ToolRegistry, Extension, ExtensionRegistry, Limits, cancellation, typed
errors — plus the OpenAI-compatible adapter and a basic benchmark suite.

Deliberately not in the kernel: durable sessions, workflow DAGs, queues,
planners, distributed scheduling, built-in memory, UI. Those belong to the
platform, to Extensions, or to hosts like the `orcacode` CLI.
