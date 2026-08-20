# Design: `kernel` and `subagent` tools

Date: 2026-08-20
Status: approved

## Motivation

Two workflows were validated experimentally against the harness from the
outside and are worth making first-class:

1. **Stateful compute.** A persistent `python3 -i` session driven through
   the generic `process` tool worked as a REPL kernel — variables survived
   across calls — but had sharp edges: multi-line code fed to the
   interactive REPL wedged it into the `...` continuation state, and the
   only recovery was to kill and respawn by hand.
2. **Subagent fan-out.** Spawning independent `orca` CLI processes
   (`--auto-approve -p "<task>"`) gave real parallel agents with their own
   loops and tool access, orchestrated by polling their output.

Both are model-invocable capabilities, so they are built as **Tools**
(siblings of `shell` and `process` in `crates/tools`), not as
`Extension`-trait loop hooks. The user-facing framing "extensions to the
harness" is satisfied by the tools being opt-in additions outside
`core_tools()`.

## Non-goals

- Generic REPL support (node, psql, ...). Python-first; the framing
  protocol below leaves room for other drivers later.
- Out-of-process subagents (spawning the `orca` binary). The in-process
  `Agent` is the primitive; process isolation stays a host concern.
- Auto-echo of expression values (IPython behavior). `print` is the
  contract.

## 1. `kernel` tool (`crates/tools/src/kernel.rs`)

### Approach: framed exec driver, not REPL parsing

The tool never talks to the interactive REPL. It spawns

```
python3 -u -c '<driver>'
```

where the driver is a small loop that:

1. reads a framed request from stdin: a header line `EXEC <byte-count>`
   followed by exactly that many bytes of UTF-8 code;
2. executes it with `exec(code, g)` against one persistent globals dict
   `g`, so state survives across calls;
3. writes the code's stdout/stderr (merged), then a sentinel line
   `<nonce> ok` or `<nonce> err` followed by the traceback on error. The
   nonce is generated fresh per kernel spawn, so output can never collide
   with it.

Byte-count framing means arbitrary multi-line code executes verbatim —
the continuation-state failure mode is structurally impossible.

### Model-facing schema

Tool name: `kernel`. Description states plainly: Python process,
variables persist across calls, nothing is auto-echoed — `print` what you
want to see.

| Param | Type | Notes |
|---|---|---|
| `code` | string | Required for `exec`. Python source to execute. |
| `action` | string, optional | `"exec"` (default) or `"reset"` (respawn fresh kernel, state lost). |
| `timeoutMs` | number, optional | Per-call cap, default 30 000, clamped to a tool-configured max. Camel case matches the `process` tool's params. |

Response JSON: `{ "output": string, "state": "ok" | "error" | "timeout",
"traceback"?: string, "restarted"?: true, "droppedBytes"?: number }`.

### Semantics

- **Lazy spawn.** The kernel starts on first `exec`; `reset` respawns
  explicitly and reports `restarted: true`.
- **Timeout → wedged → auto-respawn.** On timeout the call returns
  `state: "timeout"` and the kernel is marked wedged (the driver may
  still be executing; its stdin framing can no longer be trusted). The
  next call respawns a fresh kernel automatically and reports
  `restarted: true` so the model knows state was lost. This codifies the
  experiment's kill-and-respawn lesson as automatic recovery.
- **Driver death** (exit, killed, stdin/stdout closed) is detected on the
  next call and handled identically: respawn + `restarted: true`.
- **Output caps.** Per-call output is capped at `max_output_bytes`
  (default aligned with `process`), oldest-first drop reported via
  `droppedBytes` — same bounded-buffer semantics as `ProcessTool`.
- **Concurrency.** `Concurrency::Serial` per tool instance: one kernel,
  one stdin. Parallel `kernel` calls in a batch run in call order.
- **Lifetime.** Child killed when the tool is dropped (CancellationToken
  pattern shared with `ProcessTool`; see section 4 for the no-dangling
  guarantees). Run cancellation interrupts the in-flight call only; the
  kernel survives between calls. A cancelled in-flight exec leaves the
  kernel mid-execution, so it is marked wedged and the next call
  respawns.
- **Driver self-termination.** The driver's read loop exits on stdin
  EOF. If the CLI dies without cleanup, the closed pipe still takes the
  kernel down — a last-resort backstop, not the primary mechanism.
- **Builder config.** `working_dir`, `max_output_bytes`,
  `default_timeout` / `max_timeout`, `python` (interpreter path,
  default `python3`).

## 2. `subagent` tool (`crates/tools/src/subagent.rs`)

### Approach: in-process `Agent`, generic over `Model`

```rust
pub struct SubagentTool<M: Model + Clone + 'static> {
    model: M,
    tools: Arc<dyn Fn() -> Vec<Arc<dyn Tool>> + Send + Sync>,
    limits: Limits,               // default: conservative max_steps
    system_prompt: Option<String> // default for spawned agents
}
```

Convenience constructor `SubagentTool::new(model, &workspace)` uses
`core_tools(&ws)` as the factory. `core_tools` does not include
`subagent`; nesting is provided by the tool itself (below), never by the
factory, so depth stays controlled.

### Nesting: subagents calling subagents

`SubagentTool` carries a `depth` (its distance from the top-level agent;
the tool registered on the top-level agent has depth 0) and a shared
`SubagentDepth(Arc<AtomicU32>)` handle holding `max_depth` — the number
of subagent levels allowed. When assembling a child's tool set, the tool
appends a self-replica (`model.clone()`, same factory, same handle,
`depth + 1`) if and only if `depth + 1 < max_depth`. At the limit the
child simply has no `subagent` tool.

`max_depth` is read at spawn time from the shared handle, so a host (or
the TUI, section 3) can change it mid-session and the next spawn obeys
the new value. Range 1..=5: 1 (the default) means the top-level agent
can spawn workers but workers cannot nest; 5 is a hard cap against
runaway fan-out (each level multiplies model calls). Already-running
chains are unaffected by a lowering — the cap gates tool-set assembly,
not running agents.

### Per call

1. Build `Agent::new(self.model.clone())` with the factory's tools, the
   tool's `Limits` (default `max_steps` lower than the parent's), and the
   per-call or default system prompt. Attach a `UsageMeter`.
2. Run via `run_with_cancellation(task, ctx.cancellation.child_token())`
   — cancelling the parent run (Esc in the CLI) kills in-flight
   subagents.
3. Return `{ "answer": string, "usage": { "inputTokens": n,
   "outputTokens": n } }`. Errors (limit exhausted, model failure) come
   back as tool errors with the message.

### Model-facing schema

Tool name: `subagent`. Description: spawns an independent agent with its
own context and file/shell tools; give it a complete, self-contained
task; it returns only its final answer.

| Param | Type | Notes |
|---|---|---|
| `task` | string | Required. The complete task prompt. |
| `system_prompt` | string, optional | Overrides the tool's default for this spawn. |

`Concurrency::Parallel` — the dispatcher already fans out a batch, so the
model can launch N subagents at once and collect their answers, which is
the orchestration pattern from the experiment.

## 3. CLI wiring (`crates/cli`)

- Register `kernel` (rooted at the workspace) and `subagent` (same model
  config as the main agent, `core_tools` factory) alongside the existing
  tool set, in both interactive and headless modes.
- Add `"kernel"` and `"subagent"` to `GATED_TOOLS` in
  `crates/cli/src/approval.rs`. `kernel` executes arbitrary code (same
  class as `shell`); `subagent` spawns an agent whose inner tools run
  un-gated — one approval for the spawn, no prompt storm from inside the
  child (the in-process equivalent of the experiment's
  `--auto-approve`). This inner-ungated behavior falls out of the
  design: the `Approval` extension is registered on the parent agent
  only, and the subagent's inner `Agent` has no extensions beyond its
  `UsageMeter`.
- Headless (`-p`) keeps the existing rule: gated tools are denied unless
  `--auto-approve`.
- **Subagent nesting setting.** The TUI exposes the shared
  `SubagentDepth` handle via a `/subagents [depth]` slash command, in the
  style of `/model`: bare `/subagents` prints the current max depth,
  `/subagents 2` sets it (clamped to 1..=5, takes effect on the next
  spawn). `main.rs` creates the handle, hands one clone to the
  `SubagentTool` and one to the TUI app state — no new worker-channel
  plumbing. Startup default 1, overridable with `ORCA_SUBAGENT_DEPTH`
  (also the only knob in headless mode, which has no TUI). `/help` gains
  the command's one-liner.

## 4. Shutdown: no dangling children

Requirement: when the CLI exits — quit command, ctrl+c/ctrl+d, error,
SIGTERM, terminal window closed — every process-tool child and kernel
must be dead, including grandchildren.

Current gaps found in the code:

- `process` spawns through `sh -c "<command>"`; both `kill_on_drop` and
  the shutdown token SIGKILL only the direct child (`sh`).
  Grandchildren (a dev server, a `python3 -i` session) are orphaned.
- Children currently share the CLI's process group, so closing the
  terminal happens to SIGHUP them. Any fix that moves children into
  their own process groups removes that accident and must replace it
  deliberately.

Design (Unix; the workspace targets macOS/Linux):

1. **Own process group per child.** `Executor::build` sets
   `process_group(0)` so each spawned child leads a fresh group. Applies
   to `shell`, `process`, and the kernel (whose user code may itself
   spawn subprocesses — they land in the kernel's group).
2. **Kill the group, not the child.** Everywhere a child is killed
   (shutdown token, explicit `kill` action, run cancellation for
   `shell`, kernel reset/respawn), send SIGKILL to the group
   (`killpg`), with `child.start_kill()` retained as the non-Unix
   fallback. Managers track each child's pgid.
3. **Synchronous group-kill in `Drop`.** `Manager::drop` (and the
   kernel's equivalent) iterates live pgids and `killpg`s them directly
   — no async machinery, so cleanup is guaranteed on every path where
   destructors run: normal return from `main`, panic unwind, error
   exits.
4. **Signal handling in the CLI.** Install SIGTERM and SIGHUP handlers
   (tokio::signal) that request quit through the normal run-loop exit so
   destructors run. Interactive ctrl+c/ctrl+d already route through the
   run loop (raw mode captures them as key events). The one
   `std::process::exit` call (`--list-models`) happens before any tool
   exists and stays as is; no new `std::process::exit` calls on paths
   where tools are alive.
5. **Backstop.** The kernel driver exits on stdin EOF (section 1). This
   covers even SIGKILL of the CLI for the kernel; plain `process`
   children that ignore closed pipes are the accepted residual risk of
   an unkillable-parent scenario.

Subagent note: each `subagent` call builds fresh tool instances that are
dropped when the call returns, so anything a subagent spawned dies with
its call — subagents cannot leak processes past their own lifetime, and
transitively the CLI shutdown story is unaffected by them.

Scope note: items 1–3 modify existing `shell.rs`/`process.rs` code. This
is a targeted improvement the requirement demands, not incidental
refactoring.

## 5. Testing

- `crates/tools/tests/kernel.rs` (real `python3`; each test skips with a
  notice if the interpreter is absent):
  - state persists across `exec` calls;
  - multi-line code (function def + blank lines + call) runs verbatim;
  - stderr and tracebacks captured, `state: "error"` set;
  - timeout returns `state: "timeout"`; next call reports
    `restarted: true` and has fresh state;
  - `reset` respawns; output cap reports `droppedBytes`;
  - serial concurrency classification.
- `crates/tools/tests/subagent.rs` (uses `ScriptedModel` from
  `orca_harness_core::testing`):
  - task → inner loop → answer round-trip, usage reported;
  - inner tool calls execute against the factory's tools;
  - cancellation propagates (parent token cancels inner run);
  - `max_steps` exhaustion surfaces as a tool error;
  - parallel fan-out: N calls in one batch all complete;
  - nesting: with `max_depth = 2` a subagent's tool set includes
    `subagent` and a grandchild run completes; at `depth + 1 ==
    max_depth` the child's tool set omits it; raising the shared handle
    mid-session enables nesting on the next spawn.
- CLI: approval test additions for the two new gated names.
- Shutdown (`crates/tools/tests/tools.rs` / `kernel.rs`):
  - killing a `process` entry kills its grandchildren (spawn
    `sh -c "sleep 300 & echo $!"`, kill, assert the sleep's pid is gone);
  - dropping the tool kills all live groups (spawn, drop, assert dead);
  - kernel driver exits on stdin EOF.

## 6. Documentation

- Module docs for both tools in the same voice as `process.rs`.
- `crates/tools/src/lib.rs` crate docs and `README.md` layout section
  updated to mention `kernel` and `subagent`.
