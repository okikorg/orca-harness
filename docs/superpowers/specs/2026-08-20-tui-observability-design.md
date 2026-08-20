# Design: TUI observability for background work

Date: 2026-08-20
Status: approved

## Motivation

Two blind spots after the kernel/subagent tools landed:

1. The TUI gives no indication of how many process-tool children,
   kernels, or in-flight subagents exist right now.
2. A running subagent is a single opaque rail line — its inner tool
   activity (the whole point of watching an agent work) is invisible
   until the final answer arrives.

## Decisions (from design review)

- Nested rail shows **inner tool lines only** — no inner thinking or
  assistant text. One indent level per nesting depth.
- Counts live in the **status line**, appended after the token counts,
  each segment rendered only when nonzero.
- On completion the nested lines **collapse** back to today's single
  subagent line; the inner tool log stays **expandable** via
  `ctrl+o` / `/expand`.

## 1. Harness side: observable subagents (`crates/tools/src/subagent.rs`)

### Spawn extensions

```rust
/// Identity of one spawned inner agent.
pub struct SubagentSpawn {
    pub id: u64,              // unique across all depths (shared counter)
    pub parent_id: Option<u64>, // the spawning subagent run, None at depth 0
    pub depth: u32,           // 0 = spawned by the top-level agent
    pub call_id: String,      // the spawning agent's tool-call id
    pub task: String,
}

type SpawnExtensions =
    Arc<dyn Fn(&SubagentSpawn) -> Vec<Arc<dyn Extension>> + Send + Sync>;
```

`SubagentTool::spawn_extensions(factory)` stores the factory; each call
builds its inner agent with the factory's extensions attached
(`Agent::extension_arc`). Replicas inherit the factory and the shared id
counter (`Arc<AtomicU64>`), so grandchildren report with their own ids,
`parent_id` linking to the spawn that created them, and `depth + 1`.

Layering: the `Extension` trait lives in harness-core, which the tools
crate already depends on. The factory mechanism is general — the CLI
attaches an event stream, but any host can attach policy, truncation, or
metering to inner agents the same way.

### In-flight counter

Subagent calls increment a shared counter at start and decrement when
the call ends — via a drop guard, so errors and cancellations decrement
too. The counter is part of `BackgroundStats` (section 2) and shared
across replicas: with nesting, three live runs mean `agents 3`
regardless of depth.

## 2. `BackgroundStats` (`crates/tools/src/stats.rs`)

```rust
/// Cloneable live counters for host status displays. All methods are
/// lock-free atomics; a default instance that nobody reads costs nothing.
#[derive(Clone, Default)]
pub struct BackgroundStats { /* three Arc<AtomicUsize> */ }

impl BackgroundStats {
    pub fn processes(&self) -> usize;
    pub fn kernels(&self) -> usize;
    pub fn agents(&self) -> usize;
    // pub(crate) increment/decrement used by the tools
}
```

Adoption (builder method `stats(BackgroundStats)` on each tool):

- **`ProcessTool`**: increment on successful spawn; decrement when the
  waiter reaps the child. `Manager::drop` subtracts its remaining live
  count (a rebuilt agent drops the old tool; the shared handle must not
  leak stale counts).
- **`KernelTool`**: 0/1 — set on spawn, cleared on kill/wedge/reset/drop
  (`kill_live` and `Drop` both clear).
- **`SubagentTool`**: the in-flight counter from section 1.

Scope: the handle counts the top-level agent's tools plus subagent runs
at every depth. Processes or kernels created *inside* a subagent's own
tool set are not counted — they live and die within that subagent's
call, which is already visible as a running agent.

## 3. CLI wiring (`crates/cli`)

- `run_mode` creates one `BackgroundStats`, clones it into the
  `build_agent` closure (so tools adopt the same handle across
  model-switch rebuilds) and into `TuiConfig`.
- `build_agent` constructs `ProcessTool` and `KernelTool` directly
  (instead of taking them from `core_tools`) so it can attach the stats
  handle, registers the remaining core tools as before, and sets the
  subagent factory:

```rust
let ui = ui.clone();
subagent = subagent.spawn_extensions(Arc::new(move |spawn| {
    let ui = ui.clone();
    let (id, parent, depth, call) = (spawn.id, spawn.parent_id, spawn.depth, spawn.call_id.clone());
    vec![Arc::new(EventStream::from_fn(move |event| {
        let _ = ui.send(UiMsg::SubagentEvent {
            id, parent_id: parent, depth, call_id: call.clone(), event,
        });
    }))]
}));
```

- `msg.rs` gains the `UiMsg::SubagentEvent` variant shown above.
- Headless mode adopts the stats handle too (harmless, unread) and does
  not forward subagent events — its output contract is unchanged. With
  `--json`, forwarding inner events is future work, out of scope here.

## 4. TUI (`crates/cli/src/tui.rs`)

### Status line

```
 qwen3.5:9b · running · in 843 out 210 · procs 2 · kernel · agents 3 · esc interrupt
```

Segments appear after the token counts, only when nonzero; `kernel` is
unnumbered (it is 0 or 1). Reads `TuiConfig`'s `BackgroundStats` at draw
time; the existing spinner ticker redraws while running, and count
changes always coincide with events or the ticker, so no extra wakeups
are needed.

### Nested rail

State: `subagent_activity: HashMap<u64, SpawnActivity>` where
`SpawnActivity { call_id: String, parent_id: Option<u64>, depth: u32,
tools: Vec<ToolActivity>, done: bool }`.

- `SubagentEvent` with `ToolCall` starts an indented activity line under
  the owning rail entry: depth-0 spawns anchor to the top-level
  `pending_calls[call_id]` rail line; deeper spawns anchor under their
  parent spawn's current position. Nested rows reuse the rail's
  `├─`/`└─` branch vocabulary one level deeper (with the parent's `│`
  continuation running alongside), so ownership is visually unambiguous;
  each nesting level offsets its branches further right.
- `ToolResult` completes the line (checkmark/cross plus duration), same
  glyphs as the main rail.
- Inner `AssistantDelta`/`ReasoningDelta` events are ignored.
- The spawn's `Result` event (or the top-level `ToolResult` for the
  subagent call, whichever arrives) marks it done: nested lines leave
  the live rail, and the completed subagent line renders exactly as
  today.

### Expandable inner log

On collapse, the spawn's tool lines are appended to the subagent's
`ToolRecord` full-output view, under an `inner activity:` heading — so
`ctrl+o` / `/expand [n]` on the subagent entry shows the complete inner
tool log (call line, outcome, duration per tool). Nested spawns appear
in the same log, further indented.

Eviction: `subagent_activity` entries are removed once folded into the
tool record; nothing accumulates across turns beyond the existing
`tool_log`.

## 5. Testing

- **Tools crate** (`crates/tools/tests/subagent.rs`):
  - a recording extension captures spawn metadata: one factory call per
    spawn; correct `call_id` and `task`; a depth-2 chain yields ids with
    the grandchild's `parent_id` = child's `id` and `depth` 1;
  - events from the factory's extension fire for inner tool calls;
  - `BackgroundStats.agents()` is 1 mid-run and 0 after success, error,
    and cancellation (drop-guard test);
  - `processes()` rises on spawn, falls on kill and on tool drop;
    `kernels()` is 1 while live, 0 after reset and drop.
- **CLI** (`tui.rs` tests): synthetic `SubagentEvent` sequences assert
  rendered rows — nested indented lines while running, one extra indent
  at depth 1, collapse on completion, `/expand` output contains the
  inner log, status line shows `procs`/`kernel`/`agents` only when
  nonzero.

## Out of scope

- Inner thinking/assistant text in the rail (decided against).
- Counting processes/kernels inside subagent tool sets.
- Subagent event forwarding in headless `--json` mode.
- A `/status` detail command (status line only, per design review).
