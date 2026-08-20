# TUI Background-Work Observability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Live status-line counts of background processes/kernels/subagents, plus a nested activity rail that shows a running subagent's inner tool calls and keeps them expandable after completion.

**Architecture:** A cloneable `BackgroundStats` (three atomics) in the tools crate, adopted by `ProcessTool`/`KernelTool`/`SubagentTool`. `SubagentTool` gains a spawn-extension factory carrying `SubagentSpawn` identity (`id`, `parent_id`, `depth`, `call_id`, `task`); the CLI plugs in an `EventStream` per spawn that forwards `UiMsg::SubagentEvent` to the TUI. The TUI tracks per-spawn tool activity, renders it indented under the owning rail line, folds it into the tool record on completion for `/expand`.

**Tech Stack:** Rust, tokio, ratatui. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-20-tui-observability-design.md`

**Verified facts (do not re-derive):**
- `ToolActivity { call_line, tool_name, input, started, elapsed, output, is_error, approval }` at `tui.rs:107`; the rail renderer is `activity_lines(app, width, live)`; records for `/expand` are `ToolRecord { call_line, tool_name, output }` in `app.tool_log`; `expand_tool` renders via `view::expand_output`.
- `HarnessEvent::ToolCall { tool_call_id, tool_name, input }` and `ToolResult { tool_call_id, tool_name, output, is_error }`; `EventStream::from_fn` exists.
- The top-level `ToolResult` for a `subagent` call is always sent *after* every inner event (inner events emit inside `subagent.call`, on the same unbounded channel).
- `crates/tools` dev-deps already include `orca-harness-extensions` (tests may use `EventStream`).
- The event loop redraws on every input/UiMsg; the ticker fires only while `app.running()`.
- `ExtensionError` is at `orca_harness_core::ExtensionError`; `Agent::extension_arc` exists.

---

### Task 1: `BackgroundStats` + `ProcessTool` adoption

**Files:**
- Create: `crates/tools/src/stats.rs`
- Modify: `crates/tools/src/lib.rs` (module + export), `crates/tools/src/process.rs`
- Test: `crates/tools/tests/tools.rs`

- [ ] **Step 1: Failing test** — append to `crates/tools/tests/tools.rs` (add `BackgroundStats` to the `orca_harness_tools` import list):

```rust
#[tokio::test]
async fn process_stats_track_live_children() {
    let stats = BackgroundStats::new();
    let tool = ProcessTool::local().stats(stats.clone());
    assert_eq!(stats.processes(), 0);
    let out = tool
        .call(json!({"action": "spawn", "command": "sleep 259.1"}), &ctx())
        .await
        .unwrap();
    assert_eq!(stats.processes(), 1);
    let id = out["id"].as_str().unwrap().to_string();
    tool.call(json!({"action": "kill", "id": id}), &ctx()).await.unwrap();
    assert_eq!(stats.processes(), 0);
}

#[tokio::test]
async fn process_stats_zero_after_tool_drop() {
    let stats = BackgroundStats::new();
    {
        let tool = ProcessTool::local().stats(stats.clone());
        tool.call(json!({"action": "spawn", "command": "sleep 258.3"}), &ctx())
            .await
            .unwrap();
        assert_eq!(stats.processes(), 1);
    }
    assert_eq!(stats.processes(), 0);
}
```

- [ ] **Step 2: Run to verify compile failure** — `cargo test -p orca-harness-tools --test tools process_stats` → FAIL: `BackgroundStats` unresolved.

- [ ] **Step 3: Implement** — `crates/tools/src/stats.rs`:

```rust
//! Live counters for host status displays: how many process-tool
//! children, kernels, and in-flight subagents exist right now. Cloneable
//! and lock-free; a default instance nobody reads costs nothing. The
//! tools mutate the counters; hosts normally only read them (the
//! increment/decrement methods are public so tests and custom tools can
//! participate).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct BackgroundStats {
    processes: Arc<AtomicUsize>,
    kernels: Arc<AtomicUsize>,
    agents: Arc<AtomicUsize>,
}

impl BackgroundStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn processes(&self) -> usize {
        self.processes.load(Ordering::Relaxed)
    }
    pub fn kernels(&self) -> usize {
        self.kernels.load(Ordering::Relaxed)
    }
    pub fn agents(&self) -> usize {
        self.agents.load(Ordering::Relaxed)
    }

    pub fn inc_processes(&self) {
        self.processes.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dec_processes(&self) {
        self.processes.fetch_sub(1, Ordering::Relaxed);
    }
    pub fn inc_kernels(&self) {
        self.kernels.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dec_kernels(&self) {
        self.kernels.fetch_sub(1, Ordering::Relaxed);
    }
    pub fn inc_agents(&self) {
        self.agents.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dec_agents(&self) {
        self.agents.fetch_sub(1, Ordering::Relaxed);
    }
}
```

`lib.rs`: `mod stats;` + `pub use stats::BackgroundStats;`

`process.rs`:
- `Manager` gains `stats: BackgroundStats`; `Proc` gains `counted: std::sync::atomic::AtomicBool` (true from spawn; whoever swaps it to false performs the single decrement — waiter on reap, or `Manager::drop` for un-reaped children).
- `ProcessTool::new` initializes `stats: BackgroundStats::default()` inside the Manager literal; builder replaces the manager (safe pre-spawn):

```rust
    /// Adopt shared live counters. Call before any spawn.
    pub fn stats(mut self, stats: BackgroundStats) -> Self {
        self.manager = Arc::new(Manager {
            seq: AtomicU64::new(0),
            procs: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
            stats,
        });
        self
    }
```

- In `spawn`: `counted: AtomicBool::new(true),` in the `Proc` literal; after inserting into `procs`: `self.manager.stats.inc_processes();`. Capture `let stats = self.manager.stats.clone();` before the waiter task; in the waiter after setting the exit code:

```rust
            if p.counted.swap(false, Ordering::Relaxed) {
                stats.dec_processes();
            }
```

- In `Manager::drop`, inside the existing loop over running procs:

```rust
                if proc.counted.swap(false, Ordering::Relaxed) {
                    self.stats.dec_processes();
                }
```

Note the spawn function's settle-wait: a fast-exiting command may already be reaped (and decremented) before `spawn` returns — the test uses long sleeps so the count is deterministically 1.

- [ ] **Step 4: Run** — `cargo test -p orca-harness-tools --test tools` → PASS (kill leaked sleeps if a failure aborts early: `pkill -f "sleep 259.1"; pkill -f "sleep 258.3"`).

- [ ] **Step 5: Commit** — `git add crates/tools/src/stats.rs crates/tools/src/lib.rs crates/tools/src/process.rs crates/tools/tests/tools.rs && git commit -m "feat(tools): BackgroundStats live counters, adopted by process tool"`

---

### Task 2: `KernelTool` stats adoption

**Files:**
- Modify: `crates/tools/src/kernel.rs`
- Test: `crates/tools/tests/kernel.rs`

- [ ] **Step 1: Failing test** — append to `crates/tools/tests/kernel.rs` (import `BackgroundStats`):

```rust
#[tokio::test]
async fn kernel_stats_track_liveness() {
    require_python!();
    let stats = orca_harness_tools::BackgroundStats::new();
    {
        let k = KernelTool::new().stats(stats.clone());
        assert_eq!(stats.kernels(), 0);
        k.call(json!({"code": "a = 1"}), &ctx()).await.unwrap();
        assert_eq!(stats.kernels(), 1);
        k.call(json!({"action": "reset"}), &ctx()).await.unwrap();
        assert_eq!(stats.kernels(), 0);
        k.call(json!({"code": "a = 1"}), &ctx()).await.unwrap();
        assert_eq!(stats.kernels(), 1);
    } // drop
    assert_eq!(stats.kernels(), 0);
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p orca-harness-tools --test kernel kernel_stats` → FAIL: no `stats` method.

- [ ] **Step 3: Implement** — in `kernel.rs`:
- Field `stats: BackgroundStats` (init `BackgroundStats::default()` in `new`), builder:

```rust
    /// Adopt shared live counters.
    pub fn stats(mut self, stats: BackgroundStats) -> Self {
        self.stats = stats;
        self
    }
```

- Centralize the pgid mirror so the 0/1 count can never drift:

```rust
    /// The single place the live-pgid mirror changes; keeps the kernels
    /// counter exactly in step with it.
    fn set_live_pgid(&self, pgid: Option<u32>) {
        let mut guard = self.live_pgid.lock().unwrap();
        match (guard.is_some(), pgid.is_some()) {
            (false, true) => self.stats.inc_kernels(),
            (true, false) => self.stats.dec_kernels(),
            _ => {}
        }
        *guard = pgid;
    }
```

Replace all three direct `*self.live_pgid.lock().unwrap() = ...` writes: `spawn_kernel` → `self.set_live_pgid(pgid);`, `kill_live` → `self.set_live_pgid(None);`, the dead-kernel detection in `exec` → `self.set_live_pgid(None);`. In `Drop`, after the kill: swap the direct read for take-and-decrement:

```rust
impl Drop for KernelTool {
    fn drop(&mut self) {
        let pgid = self.live_pgid.lock().unwrap().take();
        if let Some(pgid) = pgid {
            pgroup::kill_group(pgid);
            self.stats.dec_kernels();
        }
    }
}
```

Import `crate::stats::BackgroundStats` (via `use crate::BackgroundStats;`).

- [ ] **Step 4: Run** — `cargo test -p orca-harness-tools --test kernel -- --test-threads=1` → PASS.

- [ ] **Step 5: Commit** — `git add crates/tools/src/kernel.rs crates/tools/tests/kernel.rs && git commit -m "feat(tools): kernel tool reports liveness through BackgroundStats"`

---

### Task 3: `SubagentTool` — spawn identity, spawn extensions, agents counter

**Files:**
- Modify: `crates/tools/src/subagent.rs`, `crates/tools/src/lib.rs` (exports)
- Test: `crates/tools/tests/subagent.rs`

- [ ] **Step 1: Failing tests** — append to `crates/tools/tests/subagent.rs`:

```rust
use std::sync::Mutex;

use orca_harness_core::Extension;
use orca_harness_extensions::{EventStream, HarnessEvent};
use orca_harness_tools::{BackgroundStats, SubagentSpawn};

#[tokio::test]
async fn spawn_extensions_receive_identity_and_events() {
    let spawns: Arc<Mutex<Vec<SubagentSpawn>>> = Arc::new(Mutex::new(Vec::new()));
    let events: Arc<Mutex<Vec<(u64, HarnessEvent)>>> = Arc::new(Mutex::new(Vec::new()));

    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("1", "list_dir", json!({"path": "."}))],
        "explored",
    ));
    let (ws, _dir) = temp_ws();
    let recorded = spawns.clone();
    let sink = events.clone();
    let tool = SubagentTool::new(model, &ws).spawn_extensions(Arc::new(move |spawn| {
        recorded.lock().unwrap().push(spawn.clone());
        let sink = sink.clone();
        let id = spawn.id;
        vec![Arc::new(EventStream::from_fn(move |event| {
            sink.lock().unwrap().push((id, event));
        })) as Arc<dyn Extension>]
    }));

    let tctx = ToolContext {
        call_id: "outer-call-7".into(),
        tool_name: "subagent".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    };
    tool.call(json!({"task": "explore"}), &tctx).await.unwrap();

    let spawns = spawns.lock().unwrap();
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].depth, 0);
    assert_eq!(spawns[0].parent_id, None);
    assert_eq!(spawns[0].call_id, "outer-call-7");
    assert_eq!(spawns[0].task, "explore");

    let events = events.lock().unwrap();
    assert!(events.iter().any(|(id, e)| *id == spawns[0].id
        && matches!(e, HarnessEvent::ToolCall { tool_name, .. } if tool_name == "list_dir")));
    assert!(events.iter().any(|(id, e)| *id == spawns[0].id
        && matches!(e, HarnessEvent::Result { .. })));
}

#[tokio::test]
async fn nested_spawns_link_parent_and_depth() {
    let spawns: Arc<Mutex<Vec<SubagentSpawn>>> = Arc::new(Mutex::new(Vec::new()));
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call("1", "subagent", json!({"task": "inner"}))]),
        ModelResponse::final_text("grandchild done"),
        ModelResponse::final_text("child done"),
    ]));
    let (ws, _dir) = temp_ws();
    let recorded = spawns.clone();
    let tool = SubagentTool::new(model, &ws)
        .max_depth(SubagentDepth::new(2))
        .spawn_extensions(Arc::new(move |spawn| {
            recorded.lock().unwrap().push(spawn.clone());
            Vec::new()
        }));
    tool.call(json!({"task": "outer"}), &ctx()).await.unwrap();

    let spawns = spawns.lock().unwrap();
    assert_eq!(spawns.len(), 2);
    assert_eq!(spawns[0].depth, 0);
    assert_eq!(spawns[1].depth, 1);
    assert_eq!(spawns[1].parent_id, Some(spawns[0].id));
    assert_ne!(spawns[0].id, spawns[1].id);
}

#[tokio::test]
async fn agent_count_rises_and_falls_even_on_cancel() {
    let stats = BackgroundStats::new();
    let (ws, _dir) = temp_ws();
    let tool = Arc::new(SubagentTool::new(Arc::new(StallModel), &ws).stats(stats.clone()));
    let cancel = CancellationToken::new();
    let tctx = ToolContext {
        call_id: "t".into(),
        tool_name: "subagent".into(),
        cancellation: cancel.clone(),
        deadline: None,
    };
    let running = tool.clone();
    let handle = tokio::spawn(async move { running.call(json!({"task": "stall"}), &tctx).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(stats.agents(), 1);
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await.unwrap();
    assert_eq!(stats.agents(), 0);

    // Errors decrement too: a model with no script fails immediately.
    let empty = Arc::new(ScriptedModel::new(vec![]));
    let tool = SubagentTool::new(empty, &ws).stats(stats.clone());
    let _ = tool.call(json!({"task": "x"}), &ctx()).await;
    assert_eq!(stats.agents(), 0);
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p orca-harness-tools --test subagent spawn_extensions` → FAIL: `SubagentSpawn` unresolved.

- [ ] **Step 3: Implement** — in `subagent.rs`:

```rust
/// Identity of one spawned inner agent, handed to the host's
/// spawn-extension factory.
#[derive(Clone, Debug)]
pub struct SubagentSpawn {
    /// Unique across all depths of one tool family (shared counter).
    pub id: u64,
    /// The spawn whose inner agent issued this call; `None` at depth 0.
    pub parent_id: Option<u64>,
    /// 0 = spawned by the top-level agent.
    pub depth: u32,
    /// The spawning agent's tool-call id (anchors UI rendering).
    pub call_id: String,
    pub task: String,
}

/// Builds extensions to attach to each spawned inner agent.
pub type SpawnExtensions =
    Arc<dyn Fn(&SubagentSpawn) -> Vec<Arc<dyn Extension>> + Send + Sync>;
```

Fields added to `SubagentTool` (all inherited by `child_replica`):

```rust
    spawn_extensions: Option<SpawnExtensions>,
    /// Shared across replicas so ids are unique through the whole tree.
    spawn_seq: Arc<AtomicU64>,
    /// The spawn that created this tool instance (None on the top level).
    parent_spawn: Option<u64>,
    stats: BackgroundStats,
```

(`use std::sync::atomic::AtomicU64;` and `use crate::BackgroundStats;`.) Initialize in `with_tools`: `spawn_extensions: None, spawn_seq: Arc::new(AtomicU64::new(0)), parent_spawn: None, stats: BackgroundStats::default(),`. Builders:

```rust
    /// Attach host extensions (event streams, policy, ...) to every
    /// spawned inner agent, including nested ones.
    pub fn spawn_extensions(mut self, factory: SpawnExtensions) -> Self {
        self.spawn_extensions = Some(factory);
        self
    }

    /// Adopt shared live counters (in-flight agent count, all depths).
    pub fn stats(mut self, stats: BackgroundStats) -> Self {
        self.stats = stats;
        self
    }
```

`child_replica` takes the current spawn id:

```rust
    fn child_replica(&self, spawn_id: u64) -> Self {
        Self {
            model: self.model.clone(),
            tools: self.tools.clone(),
            limits: self.limits.clone(),
            system_prompt: self.system_prompt.clone(),
            depth: self.depth + 1,
            max_depth: self.max_depth.clone(),
            spawn_extensions: self.spawn_extensions.clone(),
            spawn_seq: self.spawn_seq.clone(),
            parent_spawn: Some(spawn_id),
            stats: self.stats.clone(),
        }
    }
```

Counter guard (module-level):

```rust
/// Decrements the in-flight agent count however the call ends.
struct InFlight(BackgroundStats);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.dec_agents();
    }
}
```

In `call`, after extracting `task`/`system`:

```rust
        let spawn_id = self.spawn_seq.fetch_add(1, Ordering::SeqCst);
        self.stats.inc_agents();
        let _in_flight = InFlight(self.stats.clone());
```

and when assembling the agent (replica line changes, factory attaches after tools):

```rust
        if self.depth + 1 < self.max_depth.get() {
            agent = agent.tool_arc(Arc::new(self.child_replica(spawn_id)));
        }
        if let Some(factory) = &self.spawn_extensions {
            let spawn = SubagentSpawn {
                id: spawn_id,
                parent_id: self.parent_spawn,
                depth: self.depth,
                call_id: ctx.call_id.clone(),
                task: task.to_string(),
            };
            for extension in factory(&spawn) {
                agent = agent.extension_arc(extension);
            }
        }
```

`lib.rs` export: `pub use subagent::{SpawnExtensions, SubagentDepth, SubagentSpawn, SubagentTool, MAX_SUBAGENT_DEPTH, MIN_SUBAGENT_DEPTH};`

- [ ] **Step 4: Run** — `cargo test -p orca-harness-tools --test subagent` → PASS.

- [ ] **Step 5: Commit** — `git add crates/tools/src/subagent.rs crates/tools/src/lib.rs crates/tools/tests/subagent.rs && git commit -m "feat(tools): subagent spawn identity, host extensions, in-flight count"`

---

### Task 4: CLI plumbing — `SubagentEvent`, stats handle, factory wiring

**Files:**
- Modify: `crates/cli/src/msg.rs`, `crates/cli/src/main.rs`, `crates/cli/src/tui.rs` (TuiConfig field + literals)

No isolated test here — this task only compiles the plumbing; behavior is tested in Tasks 5-6 through the TUI. Steps:

- [ ] **Step 1: `msg.rs`** — add to `UiMsg`:

```rust
    /// A lifecycle event from inside a running subagent (any depth).
    SubagentEvent {
        id: u64,
        parent_id: Option<u64>,
        depth: u32,
        /// The spawning agent's tool-call id (anchors the rail line).
        call_id: String,
        event: HarnessEvent,
    },
```

- [ ] **Step 2: `main.rs`** — import `BackgroundStats` and `SubagentSpawn` (extend the existing `orca_harness_tools` import). In `run_mode`, next to the depth handle: `let stats = BackgroundStats::new();`, clone into the build closure and pass to `build_agent`; add `stats: stats.clone()` to the `TuiConfig` literal. `build_agent` gains `stats: &BackgroundStats` and wires everything after the `core_tools` loop (the stats-wired `ProcessTool` re-registration replaces `core_tools`' process entry by name, keeping its position):

```rust
    let root = ws.root().to_string_lossy().into_owned();
    agent = agent.tool_arc(std::sync::Arc::new(
        ProcessTool::local()
            .working_dir(root.clone())
            .stats(stats.clone()),
    ));
    agent = agent.tool_arc(std::sync::Arc::new(
        KernelTool::new().working_dir(root).stats(stats.clone()),
    ));
    let ui_events = ui.clone();
    let subagent = SubagentTool::new(model_for_subagents, ws)
        .max_depth(subagent_depth.clone())
        .stats(stats.clone())
        .spawn_extensions(std::sync::Arc::new(move |spawn: &SubagentSpawn| {
            let ui = ui_events.clone();
            let (id, parent_id, depth) = (spawn.id, spawn.parent_id, spawn.depth);
            let call_id = spawn.call_id.clone();
            vec![std::sync::Arc::new(orca_harness_extensions::EventStream::from_fn(
                move |event| {
                    let _ = ui.send(UiMsg::SubagentEvent {
                        id,
                        parent_id,
                        depth,
                        call_id: call_id.clone(),
                        event,
                    });
                },
            )) as std::sync::Arc<dyn orca_harness_core::Extension>]
        }));
    agent = agent.tool_arc(std::sync::Arc::new(subagent));
    agent
```

(`ProcessTool` joins the import list; the old plain subagent/kernel registration from the previous feature is replaced by this block.) Headless stays untouched — its tools carry default (unread) stats handles.

- [ ] **Step 3: `tui.rs`** — `TuiConfig` gains `pub stats: orca_harness_tools::BackgroundStats,`; add `stats: orca_harness_tools::BackgroundStats::new(),` to every test `TuiConfig` literal (6 sites: `test_app`, 4 render tests, `subagents_command_tests::depth_app`). Add a no-op match arm so it compiles before Task 6: in `handle_ui_msg`, `UiMsg::SubagentEvent { .. } => {}`.

- [ ] **Step 4: Verify** — `cargo test -p orca-cli` → all existing tests PASS.

- [ ] **Step 5: Commit** — `git add crates/cli/src/msg.rs crates/cli/src/main.rs crates/cli/src/tui.rs && git commit -m "feat(cli): subagent event channel and shared background stats"`

---

### Task 5: Status-line counts

**Files:**
- Modify: `crates/cli/src/tui.rs`
- Test: same file, new test module

- [ ] **Step 1: Failing test**:

```rust
#[cfg(test)]
mod stats_segment_tests {
    use super::*;

    #[test]
    fn segments_render_only_nonzero_counts() {
        let stats = orca_harness_tools::BackgroundStats::new();
        assert_eq!(stats_segments(&stats), "");
        stats.inc_processes();
        stats.inc_processes();
        stats.inc_agents();
        assert_eq!(stats_segments(&stats), " · procs 2 · agents 1");
        stats.inc_kernels();
        assert_eq!(stats_segments(&stats), " · procs 2 · kernel · agents 1");
    }
}
```

- [ ] **Step 2: Run** — `cargo test -p orca-cli segments_render` → FAIL: `stats_segments` not found.

- [ ] **Step 3: Implement** — helper above the status-line code:

```rust
/// Status-line segments for live background work; empty when idle so the
/// line stays quiet. `kernel` is unnumbered (it is 0 or 1).
fn stats_segments(stats: &orca_harness_tools::BackgroundStats) -> String {
    let mut out = String::new();
    if stats.processes() > 0 {
        out.push_str(&format!(" · procs {}", stats.processes()));
    }
    if stats.kernels() > 0 {
        out.push_str(" · kernel");
    }
    if stats.agents() > 0 {
        out.push_str(&format!(" · agents {}", stats.agents()));
    }
    out
}
```

Status line format changes to:

```rust
    let status = format!(
        " {} · {} · in {} out {}{} · {}",
        app.cfg.model_name,
        state,
        app.tokens_in,
        app.tokens_out,
        stats_segments(&app.cfg.stats),
        hint
    );
```

Keep counts fresh while idle with background processes alive — extend the ticker guard in the `run` select loop:

```rust
            _ = ticker.tick(), if app.running() || app.cfg.stats.processes() > 0 => {
```

- [ ] **Step 4: Run** — `cargo test -p orca-cli` → PASS.

- [ ] **Step 5: Commit** — `git add crates/cli/src/tui.rs && git commit -m "feat(cli): live background-work counts in the status line"`

---

### Task 6: Nested rail — live inner tool lines

**Files:**
- Modify: `crates/cli/src/tui.rs`
- Test: same file, new test module

- [ ] **Step 1: Failing test**:

```rust
#[cfg(test)]
mod nested_rail_tests {
    use super::*;
    use orca_harness_extensions::HarnessEvent;
    use serde_json::json;

    fn nested_app() -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
            subagent_depth: orca_harness_tools::SubagentDepth::new(1),
            stats: orca_harness_tools::BackgroundStats::new(),
        })
    }

    fn rail_text(app: &App) -> String {
        activity_lines(app, 120, true)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn inner_tools_render_indented_under_the_subagent_line() {
        let mut app = nested_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "explore"}),
            },
            120,
        );
        handle_subagent_event(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                input: json!({"path": "."}),
            },
        );
        let text = rail_text(&app);
        assert!(text.contains("subagent"), "rail: {text}");
        assert!(text.contains("list_dir"), "rail: {text}");
        let inner_line = text.lines().find(|l| l.contains("list_dir")).unwrap();
        assert!(
            inner_line.starts_with("      "),
            "inner line must be indented: {inner_line:?}"
        );

        handle_subagent_event(
            &mut app,
            7,
            None,
            0,
            "c1".into(),
            HarnessEvent::ToolResult {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                output: json!({"entries": []}),
                is_error: false,
            },
        );
        let text = rail_text(&app);
        let inner_line = text.lines().find(|l| l.contains("list_dir")).unwrap();
        assert!(inner_line.contains("✓"), "completed glyph: {inner_line:?}");
    }

    #[test]
    fn deeper_spawns_indent_further() {
        let mut app = nested_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "outer"}),
            },
            120,
        );
        handle_subagent_event(
            &mut app, 1, None, 0, "c1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "i1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "inner"}),
            },
        );
        handle_subagent_event(
            &mut app, 2, Some(1), 1, "i1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "g1".into(),
                tool_name: "grep".into(),
                input: json!({"pattern": "x"}),
            },
        );
        let text = rail_text(&app);
        let child = text.lines().find(|l| l.contains("subagent {\"task\":\"inner")).unwrap();
        let grandchild = text.lines().find(|l| l.contains("grep")).unwrap();
        let indent = |l: &str| l.chars().take_while(|c| *c == ' ').count();
        assert!(indent(grandchild) > indent(child), "child: {child:?} grandchild: {grandchild:?}");
    }
}
```

- [ ] **Step 2: Run** — `cargo test -p orca-cli nested_rail` → FAIL: `handle_subagent_event` not found.

- [ ] **Step 3: Implement**:

1. `ToolActivity` gains `call_id: String` as its first field; set `call_id: tool_call_id.clone(),` in `handle_harness_event`'s `ToolCall` arm (the only construction site; nested constructions below add their own).
2. New state on `App`:

```rust
    /// Live inner activity of running subagents, keyed by spawn id.
    subagent_activity: std::collections::HashMap<u64, SpawnActivity>,
```

(init in `App::new`, clear in `reset_activity` and in the `/clear` handler alongside `tool_log.clear()`), with:

```rust
/// One spawned inner agent's tool activity while it runs.
struct SpawnActivity {
    /// Tool-call id of the subagent call that spawned it.
    call_id: String,
    parent_id: Option<u64>,
    depth: u32,
    tools: Vec<ToolActivity>,
    /// Inner call id -> index into `tools`.
    pending: std::collections::HashMap<String, usize>,
}
```

3. Event handling — new function next to `handle_harness_event`, called from `handle_ui_msg` (replace the Task 4 no-op arm with `UiMsg::SubagentEvent { id, parent_id, depth, call_id, event } => handle_subagent_event(app, id, parent_id, depth, call_id, event),`):

```rust
/// Inner subagent lifecycle: only tool calls/results feed the nested
/// rail; inner deltas and text stay hidden by design.
fn handle_subagent_event(
    app: &mut App,
    id: u64,
    parent_id: Option<u64>,
    depth: u32,
    call_id: String,
    event: HarnessEvent,
) {
    match event {
        HarnessEvent::ToolCall {
            tool_call_id,
            tool_name,
            input,
        } => {
            let spawn = app
                .subagent_activity
                .entry(id)
                .or_insert_with(|| SpawnActivity {
                    call_id,
                    parent_id,
                    depth,
                    tools: Vec::new(),
                    pending: std::collections::HashMap::new(),
                });
            let call_line = view::tool_call_line(&tool_name, &input);
            let index = spawn.tools.len();
            spawn.tools.push(ToolActivity {
                call_id: tool_call_id.clone(),
                call_line,
                tool_name,
                input,
                started: Instant::now(),
                elapsed: None,
                output: None,
                is_error: false,
                approval: None,
            });
            spawn.pending.insert(tool_call_id, index);
        }
        HarnessEvent::ToolResult {
            tool_call_id,
            output,
            is_error,
            ..
        } => {
            if let Some(spawn) = app.subagent_activity.get_mut(&id) {
                if let Some(index) = spawn.pending.remove(&tool_call_id) {
                    if let Some(tool) = spawn.tools.get_mut(index) {
                        tool.elapsed = Some(tool.started.elapsed());
                        tool.output = Some(output);
                        tool.is_error = is_error;
                    }
                }
            }
        }
        _ => {}
    }
}
```

4. Rendering — inside `activity_lines`'s per-tool loop, after the `edit_file`/error extras:

```rust
        if tool.tool_name == "subagent" && tool.output.is_none() {
            nested_subagent_lines(app, &tool.call_id, width, &mut lines);
        }
```

with (near `activity_lines`):

```rust
/// Cap on rendered inner tool rows per spawn while live.
const NESTED_TOOL_ROWS: usize = 4;

/// Indented inner tool rows for every spawn anchored to `call_id`, plus
/// their descendants, one extra indent level per depth.
fn nested_subagent_lines(app: &App, call_id: &str, width: usize, lines: &mut Vec<Line<'static>>) {
    let mut roots: Vec<u64> = app
        .subagent_activity
        .iter()
        .filter(|(_, spawn)| spawn.call_id == call_id)
        .map(|(id, _)| *id)
        .collect();
    roots.sort_unstable();
    for id in roots {
        nested_spawn_rows(app, id, width, lines);
    }
}

fn nested_spawn_rows(app: &App, id: u64, width: usize, lines: &mut Vec<Line<'static>>) {
    let Some(spawn) = app.subagent_activity.get(&id) else {
        return;
    };
    let t = theme();
    let indent = " ".repeat(6 + 4 * spawn.depth as usize);
    let hidden = spawn.tools.len().saturating_sub(NESTED_TOOL_ROWS);
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("{indent}… {hidden} earlier tools"),
            t.dim,
        )));
    }
    for tool in spawn.tools.iter().skip(hidden) {
        let elapsed = tool.elapsed.unwrap_or_else(|| tool.started.elapsed());
        let (glyph, style) = match &tool.output {
            Some(_) if tool.is_error => ("×", t.error),
            Some(_) => ("✓", t.dim),
            None => ("□", t.dim),
        };
        let call = view::truncate_line(
            &tool.call_line,
            width.saturating_sub(indent.len() + 16).max(8),
        );
        lines.push(Line::from(vec![
            Span::styled(format!("{indent}{glyph} "), style),
            Span::styled(call, t.accent),
            Span::styled(format!(" · {}", elapsed_label(elapsed)), t.dim),
        ]));
        // A running nested subagent call: its spawns render below it.
        if tool.tool_name == "subagent" && tool.output.is_none() {
            let mut children: Vec<u64> = app
                .subagent_activity
                .iter()
                .filter(|(_, s)| s.parent_id == Some(id))
                .map(|(child, _)| *child)
                .collect();
            children.sort_unstable();
            for child in children {
                nested_spawn_rows(app, child, width, lines);
            }
        }
    }
}
```

- [ ] **Step 4: Run** — `cargo test -p orca-cli` → PASS (both nested tests plus all existing).

- [ ] **Step 5: Commit** — `git add crates/cli/src/tui.rs && git commit -m "feat(cli): nested activity rail for running subagents"`

---

### Task 7: Collapse + expandable inner log

**Files:**
- Modify: `crates/cli/src/tui.rs`
- Test: same file, extend `nested_rail_tests`

- [ ] **Step 1: Failing test** — append inside `nested_rail_tests`:

```rust
    #[test]
    fn completion_folds_inner_log_into_the_expandable_record() {
        let mut app = nested_app();
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolCall {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                input: json!({"task": "explore"}),
            },
            120,
        );
        handle_subagent_event(
            &mut app, 7, None, 0, "c1".into(),
            HarnessEvent::ToolCall {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                input: json!({"path": "."}),
            },
        );
        handle_subagent_event(
            &mut app, 7, None, 0, "c1".into(),
            HarnessEvent::ToolResult {
                tool_call_id: "i1".into(),
                tool_name: "list_dir".into(),
                output: json!({"entries": []}),
                is_error: false,
            },
        );
        handle_harness_event(
            &mut app,
            HarnessEvent::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "subagent".into(),
                output: json!({"answer": "found things"}),
                is_error: false,
            },
            120,
        );

        assert!(app.subagent_activity.is_empty(), "spawn state must fold away");
        let record = app.tool_log.last().unwrap();
        assert!(record.inner.iter().any(|l| l.contains("list_dir")));

        expand_tool(&mut app, 1, 120);
        let expanded: String = app.pending_history.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(expanded.contains("inner activity"), "{expanded}");
        assert!(expanded.contains("list_dir"), "{expanded}");
    }
```

- [ ] **Step 2: Run** — `cargo test -p orca-cli completion_folds` → FAIL: no `inner` field.

- [ ] **Step 3: Implement**:

1. `ToolRecord` gains `inner: Vec<String>,`; existing literals (`flush_reasoning`, `handle_harness_event` ToolResult arm) add `inner: Vec::new(),`.
2. In `handle_harness_event`'s `ToolResult` arm, fold before pushing the record:

```rust
            let inner = if tool_name == "subagent" {
                fold_subagent_activity(app, &tool_call_id)
            } else {
                Vec::new()
            };
            app.push_record(ToolRecord {
                call_line,
                tool_name,
                output,
                inner,
            });
```

with:

```rust
/// Remove all spawn activity anchored to a finished subagent call
/// (including nested descendants) and render it to plain lines for the
/// expandable record.
fn fold_subagent_activity(app: &mut App, call_id: &str) -> Vec<String> {
    let roots: Vec<u64> = app
        .subagent_activity
        .iter()
        .filter(|(_, spawn)| spawn.call_id == call_id)
        .map(|(id, _)| *id)
        .collect();
    let mut lines = Vec::new();
    let mut sorted = roots;
    sorted.sort_unstable();
    for id in sorted {
        collect_spawn_log(app, id, &mut lines);
    }
    lines
}

fn collect_spawn_log(app: &mut App, id: u64, lines: &mut Vec<String>) {
    let Some(spawn) = app.subagent_activity.remove(&id) else {
        return;
    };
    let indent = "  ".repeat(spawn.depth as usize);
    for tool in &spawn.tools {
        let glyph = match &tool.output {
            Some(_) if tool.is_error => "×",
            Some(_) => "✓",
            None => "□",
        };
        let elapsed = tool.elapsed.unwrap_or_default();
        lines.push(format!(
            "{indent}{glyph} {} · {}",
            tool.call_line,
            elapsed_label(elapsed)
        ));
    }
    let mut children: Vec<u64> = app
        .subagent_activity
        .iter()
        .filter(|(_, s)| s.parent_id == Some(id))
        .map(|(child, _)| *child)
        .collect();
    children.sort_unstable();
    for child in children {
        collect_spawn_log(app, child, lines);
    }
}
```

3. `expand_tool` appends the inner section before the closing `└` line:

```rust
    if !record.inner.is_empty() {
        rendered.push(Line::from(vec![
            Span::styled("  │ ", t.dim),
            Span::styled("inner activity:", t.dim),
        ]));
        for line in &record.inner {
            rendered.push(Line::from(vec![
                Span::styled("  │   ", t.dim),
                Span::raw(view::truncate_line(line, body_width.saturating_sub(2))),
            ]));
        }
    }
```

(borrow note: `expand_tool` currently holds `record` as a shared borrow of `app.tool_log` while pushing to `app.pending_history` at the end — the existing code already collects into a local `rendered` first, so extend `rendered` and keep the final `app.pending_history.extend(rendered)` last.)

- [ ] **Step 4: Run** — `cargo test -p orca-cli` → PASS.

- [ ] **Step 5: Commit** — `git add crates/cli/src/tui.rs && git commit -m "feat(cli): fold finished subagent activity into the expandable record"`

---

### Task 8: Docs + full verification

**Files:**
- Modify: `README.md`

- [ ] **Step 1: README** — in the interactive-mode paragraph, after the gated-tools sentence, add:

```text
Running subagents show their inner tool calls as an indented nested rail
(collapsed into the expandable record when they finish), and the status
line counts live background work (`procs 2 · kernel · agents 3`).
```

- [ ] **Step 2: Full verification** — `cargo test --workspace` → all green; `cargo clippy --workspace --all-targets` → no new warnings.

- [ ] **Step 3: Commit** — `git add README.md && git commit -m "docs: background-work observability in README"`

---

## Self-Review Notes

- **Spec coverage:** BackgroundStats + adoption (Tasks 1-2), spawn identity/extensions/counter (Task 3), CLI channel + wiring (Task 4), status line incl. idle ticker (Task 5), nested rail with depth indent + caps (Task 6), collapse + expand (Task 7), docs (Task 8). Headless: covered by default handles, no changes (spec section 3).
- **Type consistency:** `SubagentSpawn` fields, `stats_segments`, `handle_subagent_event(app, id, parent_id, depth, call_id, event)`, `SpawnActivity`, `ToolRecord.inner`, `ToolActivity.call_id` used consistently across tasks.
- **Ordering guarantee** (inner events precede the top-level ToolResult) justifies folding in the ToolResult arm — stated in Verified facts.
