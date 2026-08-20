# Kernel and Subagent Tools Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a stateful Python `kernel` tool and an in-process `subagent` tool to `crates/tools`, harden child-process shutdown with process groups, and wire both into the `orca` CLI with a TUI-adjustable subagent nesting depth.

**Architecture:** Both capabilities are ordinary `Tool` implementations (spec: `docs/superpowers/specs/2026-08-20-kernel-subagent-tools-design.md`). The kernel drives a framed Python exec driver over stdin (never the interactive REPL). The subagent builds a fresh in-process `Agent` per call, generic over `Model + Clone`, and self-replicates into children's tool sets while `depth + 1 < max_depth` (shared `Arc<AtomicU32>`). Shutdown hardening: every spawned child leads its own process group; kills target the group; `Drop` kills groups synchronously; the TUI catches SIGTERM/SIGHUP.

**Tech Stack:** Rust (workspace at repo root), tokio, no new crate dependencies (killpg via a local `extern "C"` declaration, matching the house style of the hand-rolled `either`).

**Conventions:**
- Run tests from the workspace root `/Users/akashswamy/Workspace/orca-harness`.
- Tool params are camelCase (`timeoutMs`, `systemPrompt`) matching `process`'s `waitMs`.
- Tests needing `python3` skip with an eprintln notice when it is absent.
- Commit after every task.

**Verified facts about the codebase (do not re-derive):**
- `pub trait Model: Send + Sync`; `impl<M: Model + ?Sized> Model for Arc<M>` exists (`harness-core/src/model.rs:128`), so `Arc<dyn Model>` and `Arc<ScriptedModel>` satisfy `Model + Clone`.
- `Agent::run_with_cancellation` creates a fresh `Context`, pushes the agent's `system_prompt`, then the user prompt.
- `Workspace` is `Clone`; `ws.root() -> &Path`.
- `ToolError::msg(impl Into<String>)` exists; `ToolResult`, `Concurrency::{Parallel, Serial, Keyed}` as in `harness-core/src/tool.rs`.
- Unknown tools produce an error result: `format!("unknown tool: {}", call.name)` (`dispatcher.rs:90`).
- `Usage` is `Copy` with an `add(&mut self, &Usage)` method; `ModelResponse::usage() -> Option<&Usage>`.
- `orca-cli`'s tokio dep already enables the `signal` feature.
- Test helpers in `crates/tools/tests/tools.rs`: `temp_ws()`, `ctx()`, `RUN_TIMEOUT` — copy the same helpers into new test files (integration test files cannot share code unless a common module is made; just duplicate the ~15 lines).

---

### Task 1: Process groups — `pgroup` module and `Executor` spawn change

**Files:**
- Create: `crates/tools/src/pgroup.rs`
- Modify: `crates/tools/src/lib.rs` (add `mod pgroup;`)
- Modify: `crates/tools/src/shell.rs` (`Executor::build`)

- [ ] **Step 1: Write the failing test** — append to `crates/tools/src/pgroup.rs` (module + its unit test together, since the module is `pub(crate)`):

```rust
//! Process-group helpers: every spawned child leads its own group so a
//! kill reaches grandchildren (`sh -c` wrappers, servers they fork).
//! No `libc` dependency: `killpg` is declared directly, in the same
//! spirit as the hand-rolled `either` in `process.rs`.

#[cfg(unix)]
mod imp {
    extern "C" {
        fn killpg(pgrp: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;

    /// SIGKILL every process in `pgid`'s group. Best-effort: the group
    /// may already be gone.
    pub fn kill_group(pgid: u32) {
        unsafe {
            killpg(pgid as i32, SIGKILL);
        }
    }
}

#[cfg(not(unix))]
mod imp {
    /// Non-Unix fallback: no process groups; callers also keep
    /// `kill_on_drop`/`start_kill` which reach the direct child.
    pub fn kill_group(_pgid: u32) {}
}

pub(crate) use imp::kill_group;

#[cfg(all(test, unix))]
mod tests {
    use super::kill_group;

    #[tokio::test]
    async fn kill_group_reaches_grandchildren() {
        // sh spawns sleep as its own child; both live in sh's group.
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 283.1 & wait"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let found = std::process::Command::new("pgrep")
            .args(["-f", "sleep 283.1"])
            .output()
            .unwrap();
        assert!(found.status.success(), "grandchild should be running");

        kill_group(pgid);
        let _ = child.wait().await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let found = std::process::Command::new("pgrep")
            .args(["-f", "sleep 283.1"])
            .output()
            .unwrap();
        assert!(!found.status.success(), "grandchild must be dead after group kill");
    }
}
```

Register the module in `crates/tools/src/lib.rs` next to the other `mod` lines:

```rust
mod pgroup;
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p orca-harness-tools pgroup -- --nocapture`
Expected: PASS (the module and test land together; the failing-state check here is that the file compiles and the test genuinely exercises group kill — if it fails, the helper is wrong, fix before proceeding).

- [ ] **Step 3: Make every `Executor`-built command lead its own group** — in `crates/tools/src/shell.rs`, `Executor::build`, before `cmd`'s return:

```rust
        if self.is_local_sh() {
            if let Some(dir) = working_dir {
                cmd.current_dir(dir);
            }
        }
        // Own process group: kills can reach grandchildren, and children
        // no longer die accidentally with the host's terminal — hosts
        // must kill them deliberately (see pgroup + CLI signal handling).
        #[cfg(unix)]
        cmd.process_group(0);
        cmd
```

- [ ] **Step 4: Run the full tools suite to confirm nothing regressed**

Run: `cargo test -p orca-harness-tools`
Expected: PASS (all existing shell/process tests still green).

- [ ] **Step 5: Commit**

```bash
git add crates/tools/src/pgroup.rs crates/tools/src/lib.rs crates/tools/src/shell.rs
git commit -m "feat(tools): spawn executor children in their own process groups"
```

---

### Task 2: Group-kill in `shell` timeout/cancel paths

**Files:**
- Modify: `crates/tools/src/shell.rs` (`ShellTool::call`)
- Test: `crates/tools/tests/tools.rs`

- [ ] **Step 1: Write the failing test** — append to `crates/tools/tests/tools.rs`:

```rust
#[cfg(unix)]
#[tokio::test]
async fn shell_timeout_kills_grandchildren() {
    let tool = ShellTool::local().timeout(Some(Duration::from_millis(400)));
    let err = tool
        .call(json!({"command": "sleep 281.7 & wait"}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let found = std::process::Command::new("pgrep")
        .args(["-f", "sleep 281.7"])
        .output()
        .unwrap();
    assert!(
        !found.status.success(),
        "grandchild sleep must die with the timed-out shell call"
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p orca-harness-tools shell_timeout_kills_grandchildren`
Expected: FAIL on the final assert — `kill_on_drop` kills only `sh`; the backgrounded `sleep` survives.

- [ ] **Step 3: Kill the group on cancel and timeout** — in `crates/tools/src/shell.rs`:

Add the import:

```rust
use crate::pgroup;
```

In `ShellTool::call`, capture the pgid right after spawn (before pipes are taken):

```rust
        let mut child = self
            .build_command(command_str)
            .spawn()
            .map_err(|e| ToolError::msg(format!("failed to spawn: {e}")))?;
        let pgid = child.id();
```

Then kill the group in both early-exit arms:

```rust
        let outcome = tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => {
                if let Some(pgid) = pgid {
                    pgroup::kill_group(pgid);
                }
                return Err(ToolError::msg("cancelled"));
            }
            result = async {
                match self.timeout {
                    Some(t) => tokio::time::timeout(t, read_streams).await.map_err(|_| ()),
                    None => Ok(read_streams.await),
                }
            } => result,
        };

        let (stdout, stderr, status) = match outcome {
            Ok(triple) => triple,
            Err(()) => {
                if let Some(pgid) = pgid {
                    pgroup::kill_group(pgid);
                }
                return Err(ToolError::msg("command timed out"));
            }
        };
```

Also update the stale comment above the select (it said `kill_on_drop` handles it):

```rust
        // Race execution against cancellation and the optional timeout.
        // On either, the whole process group is SIGKILLed so grandchildren
        // die too; `kill_on_drop` remains as the non-Unix fallback.
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p orca-harness-tools shell_timeout_kills_grandchildren`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/tools/src/shell.rs crates/tools/tests/tools.rs
git commit -m "feat(tools): shell kills the whole process group on timeout and cancel"
```

---

### Task 3: Group-kill in `process` — pgid tracking, waiter, and synchronous `Drop`

**Files:**
- Modify: `crates/tools/src/process.rs`
- Test: `crates/tools/tests/tools.rs`

- [ ] **Step 1: Write the failing tests** — append to `crates/tools/tests/tools.rs`:

```rust
#[cfg(unix)]
#[tokio::test]
async fn process_kill_reaches_grandchildren() {
    let tool = ProcessTool::local();
    let out = tool
        .call(json!({"action": "spawn", "command": "sleep 279.3 & wait"}), &ctx())
        .await
        .unwrap();
    let id = out["id"].as_str().unwrap().to_string();
    let found = std::process::Command::new("pgrep")
        .args(["-f", "sleep 279.3"])
        .output()
        .unwrap();
    assert!(found.status.success(), "grandchild should be running");

    tool.call(json!({"action": "kill", "id": id}), &ctx())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let found = std::process::Command::new("pgrep")
        .args(["-f", "sleep 279.3"])
        .output()
        .unwrap();
    assert!(!found.status.success(), "grandchild must die with the kill action");
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_process_tool_kills_children_synchronously() {
    {
        let tool = ProcessTool::local();
        tool.call(json!({"action": "spawn", "command": "sleep 277.9 & wait"}), &ctx())
            .await
            .unwrap();
    } // tool dropped here
    tokio::time::sleep(Duration::from_millis(300)).await;
    let found = std::process::Command::new("pgrep")
        .args(["-f", "sleep 277.9"])
        .output()
        .unwrap();
    assert!(!found.status.success(), "children must die when the tool is dropped");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p orca-harness-tools process_kill_reaches_grandchildren dropping_process_tool_kills_children_synchronously`
Expected: both FAIL — only `sh` dies today, the backgrounded `sleep` survives.

- [ ] **Step 3: Track pgids and kill groups** — in `crates/tools/src/process.rs`:

Add the import:

```rust
use crate::pgroup;
```

Add a `pgid` field to `Proc`:

```rust
struct Proc {
    command: String,
    /// Process-group id (== child pid, since each child leads its group).
    pgid: Option<u32>,
    buf: Mutex<OutBuf>,
    ...existing fields unchanged...
}
```

In `spawn`, capture the pid before the pipes are taken and store it:

```rust
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::msg(format!("failed to spawn: {e}")))?;
        let pgid = child.id();
```

and in the `Proc` construction: `pgid,` (alongside `command: command_str.to_string(),`).

In the waiter task, kill the group on the cancel branch:

```rust
            let status = tokio::select! {
                biased;
                _ = p.kill.cancelled() => {
                    if let Some(pgid) = p.pgid {
                        pgroup::kill_group(pgid);
                    }
                    let _ = child.start_kill();
                    child.wait().await
                }
                status = child.wait() => status,
            };
```

Make `Manager::drop` kill groups synchronously (works even mid runtime-shutdown, when spawned waiter tasks may never run again):

```rust
impl Drop for Manager {
    fn drop(&mut self) {
        // Cancel wakes the waiters if the runtime still lives; the direct
        // group kills guarantee cleanup even when it does not.
        self.shutdown.cancel();
        for proc in self.procs.lock().unwrap().values() {
            if proc.running() {
                if let Some(pgid) = proc.pgid {
                    pgroup::kill_group(pgid);
                }
            }
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass, then the whole suite**

Run: `cargo test -p orca-harness-tools`
Expected: PASS, including the two new tests.

- [ ] **Step 5: Commit**

```bash
git add crates/tools/src/process.rs crates/tools/tests/tools.rs
git commit -m "feat(tools): process tool kills whole groups, synchronously on drop"
```

---

### Task 4: `kernel` tool — driver, framing, exec happy path

**Files:**
- Create: `crates/tools/src/kernel.rs`
- Modify: `crates/tools/src/lib.rs` (register module + export + docs mention later in Task 10)
- Test: `crates/tools/tests/kernel.rs`

- [ ] **Step 1: Write the failing tests** — create `crates/tools/tests/kernel.rs`:

```rust
//! Kernel tool against a real python3. Every test skips (with a notice)
//! when python3 is absent.

use serde_json::json;

use orca_harness_core::{CancellationToken, Concurrency, Tool, ToolContext};
use orca_harness_tools::KernelTool;

fn ctx() -> ToolContext {
    ToolContext {
        call_id: "t".into(),
        tool_name: "kernel".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

macro_rules! require_python {
    () => {
        if !python3_available() {
            eprintln!("skipping: python3 not found on PATH");
            return;
        }
    };
}

#[tokio::test]
async fn state_persists_across_calls() {
    require_python!();
    let k = KernelTool::new();
    let out = k.call(json!({"code": "x = 41"}), &ctx()).await.unwrap();
    assert_eq!(out["state"], "ok");
    assert_eq!(out["output"], "");
    let out = k.call(json!({"code": "print(x + 1)"}), &ctx()).await.unwrap();
    assert_eq!(out["state"], "ok");
    assert_eq!(out["output"].as_str().unwrap().trim(), "42");
}

#[tokio::test]
async fn multiline_code_runs_verbatim() {
    require_python!();
    let k = KernelTool::new();
    // Blank lines inside a def are exactly what wedged the interactive REPL.
    let code = "def double(n):\n\n    return n * 2\n\nprint(double(21))";
    let out = k.call(json!({"code": code}), &ctx()).await.unwrap();
    assert_eq!(out["state"], "ok");
    assert_eq!(out["output"].as_str().unwrap().trim(), "42");
}

#[tokio::test]
async fn errors_report_traceback_and_preserve_state() {
    require_python!();
    let k = KernelTool::new();
    k.call(json!({"code": "kept = 'alive'"}), &ctx()).await.unwrap();
    let out = k.call(json!({"code": "1 / 0"}), &ctx()).await.unwrap();
    assert_eq!(out["state"], "error");
    assert!(out["traceback"]
        .as_str()
        .unwrap()
        .contains("ZeroDivisionError"));
    // An exception must not cost the session its state.
    let out = k.call(json!({"code": "print(kept)"}), &ctx()).await.unwrap();
    assert_eq!(out["output"].as_str().unwrap().trim(), "alive");
}

#[tokio::test]
async fn stderr_is_captured_in_order() {
    require_python!();
    let k = KernelTool::new();
    let out = k
        .call(
            json!({"code": "import sys\nprint('a')\nprint('b', file=sys.stderr)\nprint('c')"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["output"].as_str().unwrap().trim(), "a\nb\nc");
}

#[test]
fn kernel_calls_are_serial() {
    let k = KernelTool::new();
    assert_eq!(k.concurrency(&json!({"code": "1"})), Concurrency::Serial);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p orca-harness-tools --test kernel`
Expected: FAIL to compile — `KernelTool` does not exist.

- [ ] **Step 3: Implement the kernel core** — create `crates/tools/src/kernel.rs`:

```rust
//! `kernel` — stateful Python compute.
//!
//! Where `process` can drive `python3 -i` line by line, the interactive
//! REPL wedges on multi-line input. The kernel never talks to the REPL:
//! it spawns `python3 -u -c '<driver>' <nonce>` where the driver reads
//! byte-count-framed code blocks from stdin, runs each with `exec`
//! against one persistent globals dict, and delimits every execution's
//! output with a per-spawn sentinel line. Arbitrary multi-line code runs
//! verbatim; variables persist across calls.
//!
//! Recovery: a timed-out or cancelled execution leaves the driver in an
//! unknown spot, so the kernel is killed (whole process group) and the
//! next call respawns fresh, reporting `restarted: true` — state loss is
//! explicit, never silent. The driver also exits on stdin EOF, so a dead
//! host cannot leave a kernel behind.

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout};

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use crate::pgroup;

/// The in-process side of the framing protocol. `os.dup2(1, 2)` merges
/// stderr into stdout at the fd level so ordering is preserved even for
/// subprocesses the user's code spawns.
const DRIVER: &str = r#"
import sys, os, traceback
os.dup2(1, 2)
g = {'__name__': '__main__'}
n = sys.argv[1]
while True:
    h = sys.stdin.buffer.readline()
    if not h:
        break
    h = h.strip()
    if not h.startswith(b'EXEC '):
        continue
    k = int(h[5:])
    b = sys.stdin.buffer.read(k)
    while len(b) < k:
        m = sys.stdin.buffer.read(k - len(b))
        if not m:
            break
        b += m
    try:
        exec(compile(b.decode('utf-8'), '<kernel>', 'exec'), g)
        sys.stdout.flush()
        sys.stdout.write('\n%s ok\n' % n)
    except BaseException:
        sys.stdout.flush()
        sys.stdout.write('\n%s err\n' % n)
        sys.stdout.write(traceback.format_exc())
        sys.stdout.write('%s end\n' % n)
    sys.stdout.flush()
"#;

struct Live {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    nonce: String,
    pgid: Option<u32>,
}

#[derive(Default)]
struct Session {
    live: Option<Live>,
    /// The previous kernel died losing state (wedge, crash, cancel); the
    /// next successful spawn reports `restarted: true`.
    restart_notice: bool,
}

pub struct KernelTool {
    python: String,
    working_dir: Option<String>,
    max_output_bytes: usize,
    default_timeout: Duration,
    max_timeout: Duration,
    session: tokio::sync::Mutex<Session>,
    /// Mirror of the live kernel's pgid, readable from the sync `Drop`.
    live_pgid: StdMutex<Option<u32>>,
    seq: AtomicU64,
}

impl Default for KernelTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for KernelTool {
    fn drop(&mut self) {
        if let Some(pgid) = *self.live_pgid.lock().unwrap() {
            pgroup::kill_group(pgid);
        }
    }
}

impl KernelTool {
    pub fn new() -> Self {
        Self {
            python: "python3".into(),
            working_dir: None,
            max_output_bytes: 64 * 1024,
            default_timeout: Duration::from_secs(30),
            max_timeout: Duration::from_secs(300),
            session: tokio::sync::Mutex::new(Session::default()),
            live_pgid: StdMutex::new(None),
            seq: AtomicU64::new(0),
        }
    }

    pub fn python(mut self, interpreter: impl Into<String>) -> Self {
        self.python = interpreter.into();
        self
    }

    pub fn working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    pub fn max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    pub fn default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    fn spawn_kernel(&self) -> Result<Live, ToolError> {
        let nonce = format!(
            "ORCA_K_{}_{}",
            std::process::id(),
            self.seq.fetch_add(1, Ordering::SeqCst)
        );
        let mut cmd = tokio::process::Command::new(&self.python);
        cmd.args(["-u", "-c", DRIVER, &nonce])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(dir) = &self.working_dir {
            cmd.current_dir(dir);
        }
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::msg(format!("failed to spawn {}: {e}", self.python)))?;
        let pgid = child.id();
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        *self.live_pgid.lock().unwrap() = pgid;
        Ok(Live {
            child,
            stdin,
            stdout,
            nonce,
            pgid,
        })
    }

    /// Kill the live kernel (whole group) and reap it.
    async fn kill_live(&self, session: &mut Session) {
        if let Some(mut live) = session.live.take() {
            if let Some(pgid) = live.pgid {
                pgroup::kill_group(pgid);
            }
            let _ = live.child.start_kill();
            let _ = live.child.wait().await;
        }
        *self.live_pgid.lock().unwrap() = None;
    }

    async fn exec(&self, input: &Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let code = input
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`code` (string) is required for exec"))?;
        let timeout = input
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(self.default_timeout)
            .min(self.max_timeout);

        let mut session = self.session.lock().await;

        // A kernel that exited on its own (crash, os._exit) lost state too.
        if let Some(live) = &mut session.live {
            if live.child.try_wait().ok().flatten().is_some() {
                session.live = None;
                session.restart_notice = true;
                *self.live_pgid.lock().unwrap() = None;
            }
        }
        let restarted = session.restart_notice && session.live.is_none();
        if session.live.is_none() {
            session.live = Some(self.spawn_kernel()?);
            session.restart_notice = false;
        }
        let live = session.live.as_mut().expect("just ensured");

        let header = format!("EXEC {}\n", code.len());
        let write = async {
            live.stdin.write_all(header.as_bytes()).await?;
            live.stdin.write_all(code.as_bytes()).await?;
            live.stdin.flush().await
        };
        if write.await.is_err() {
            // Broken pipe: kernel died mid-write. Recover next call.
            self.kill_live(&mut session).await;
            session.restart_notice = true;
            return Err(ToolError::msg(
                "kernel stdin closed; it will restart on the next call",
            ));
        }

        // Read until the sentinel, bounded by timeout and cancellation.
        let ok_mark = format!("\n{} ok\n", live.nonce).into_bytes();
        let err_mark = format!("\n{} err\n", live.nonce).into_bytes();
        let end_mark = format!("{} end\n", live.nonce).into_bytes();
        let mut buf: Vec<u8> = Vec::new();
        let mut dropped: u64 = 0;
        // Retain enough to always find a sentinel that straddles reads.
        let keep = self.max_output_bytes + ok_mark.len().max(err_mark.len()) + end_mark.len() + 256;

        let outcome = loop {
            if let Some(pos) = find(&buf, &ok_mark) {
                break Some(("ok", pos, None));
            }
            if let Some(pos) = find(&buf, &err_mark) {
                let tb_start = pos + err_mark.len();
                if let Some(end) = find(&buf[tb_start..], &end_mark) {
                    let traceback =
                        String::from_utf8_lossy(&buf[tb_start..tb_start + end]).into_owned();
                    break Some(("error", pos, Some(traceback)));
                }
            }
            let mut chunk = [0u8; 8192];
            let read = tokio::select! {
                biased;
                _ = ctx.cancellation.cancelled() => break None,
                read = tokio::time::timeout(timeout, live.stdout.read(&mut chunk)) => read,
            };
            match read {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break None, // EOF, error, or timeout
                Ok(Ok(n)) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > keep {
                        let excess = buf.len() - keep;
                        buf.drain(..excess);
                        dropped += excess as u64;
                    }
                }
            }
        };

        match outcome {
            Some((state, pos, traceback)) => {
                let mut output = &buf[..pos];
                // The driver writes one '\n' before the sentinel to force a
                // line boundary; hide that seam. (`pos` marks the sentinel's
                // leading '\n', so output already excludes it.)
                if output.len() > self.max_output_bytes {
                    let excess = output.len() - self.max_output_bytes;
                    dropped += excess as u64;
                    output = &output[excess..];
                }
                let mut out = json!({
                    "state": state,
                    "output": String::from_utf8_lossy(output).into_owned(),
                });
                if let Some(tb) = traceback {
                    out["traceback"] = json!(tb);
                }
                if restarted {
                    out["restarted"] = json!(true);
                }
                if dropped > 0 {
                    out["droppedBytes"] = json!(dropped);
                }
                Ok(out)
            }
            None => {
                // Timeout, cancellation, or a dead driver: the framing can
                // no longer be trusted. Kill now, respawn on the next call.
                let cancelled = ctx.cancellation.is_cancelled();
                self.kill_live(&mut session).await;
                session.restart_notice = true;
                if cancelled {
                    return Err(ToolError::msg("cancelled; kernel will restart on the next call"));
                }
                let mut output = &buf[..];
                if output.len() > self.max_output_bytes {
                    output = &output[output.len() - self.max_output_bytes..];
                }
                Ok(json!({
                    "state": "timeout",
                    "output": String::from_utf8_lossy(output).into_owned(),
                    "error": format!(
                        "execution exceeded {}ms; the kernel was killed and will restart (state lost) on the next call",
                        timeout.as_millis()
                    ),
                }))
            }
        }
    }

    async fn reset(&self) -> Result<Value, ToolError> {
        let mut session = self.session.lock().await;
        self.kill_live(&mut session).await;
        session.restart_notice = false; // explicit reset: the model asked
        Ok(json!({"state": "ok", "restarted": true, "output": ""}))
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[async_trait]
impl Tool for KernelTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "kernel".into(),
            description: "Execute Python in a persistent kernel: variables, imports, and \
                functions survive across calls, so build state incrementally and reuse it. \
                Multi-line code is fine. Nothing is auto-echoed — `print()` what you want \
                to see. `reset` discards all state. A timed-out execution kills the kernel; \
                the next call starts fresh and reports `restarted: true`."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "Python source to execute (required for exec)."},
                    "action": {"type": "string", "enum": ["exec", "reset"], "default": "exec"},
                    "timeoutMs": {"type": "integer", "description": "Per-call execution cap in milliseconds (default 30000)."}
                }
            }),
        }
    }

    /// One kernel, one stdin: calls serialize in call order.
    fn concurrency(&self, _input: &Value) -> Concurrency {
        Concurrency::Serial
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        match input.get("action").and_then(Value::as_str) {
            None | Some("exec") => self.exec(&input, ctx).await,
            Some("reset") => self.reset().await,
            Some(other) => Err(ToolError::msg(format!(
                "unknown action `{other}`; expected exec|reset"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DRIVER;
    use std::process::Stdio;

    /// The no-dangling backstop: a driver whose stdin closes must exit on
    /// its own, even if nobody kills it.
    #[tokio::test]
    async fn driver_exits_on_stdin_eof() {
        let probe = std::process::Command::new("python3").arg("--version").output();
        if !probe.is_ok_and(|o| o.status.success()) {
            eprintln!("skipping: python3 not found on PATH");
            return;
        }
        let mut child = tokio::process::Command::new("python3")
            .args(["-u", "-c", DRIVER, "NONCE"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        drop(child.stdin.take()); // EOF
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .expect("driver must exit on stdin EOF")
            .unwrap();
        assert!(status.success());
    }
}
```

Register in `crates/tools/src/lib.rs`:

```rust
mod kernel;
```
and
```rust
pub use kernel::KernelTool;
```

- [ ] **Step 4: Run the kernel tests**

Run: `cargo test -p orca-harness-tools --test kernel && cargo test -p orca-harness-tools kernel`
Expected: PASS (integration tests + the in-crate `driver_exits_on_stdin_eof`).

- [ ] **Step 5: Commit**

```bash
git add crates/tools/src/kernel.rs crates/tools/src/lib.rs crates/tools/tests/kernel.rs
git commit -m "feat(tools): kernel tool — persistent Python via framed exec driver"
```

---

### Task 5: `kernel` timeout → wedge → auto-respawn, and reset

**Files:**
- Test: `crates/tools/tests/kernel.rs`
- Modify: `crates/tools/src/kernel.rs` (only if a test exposes a bug — the Task 4 implementation already contains the logic; these tests verify it)

- [ ] **Step 1: Write the tests** — append to `crates/tools/tests/kernel.rs`:

```rust
#[tokio::test]
async fn timeout_kills_kernel_and_next_call_restarts_fresh() {
    require_python!();
    let k = KernelTool::new();
    k.call(json!({"code": "y = 7"}), &ctx()).await.unwrap();
    let out = k
        .call(
            json!({"code": "import time\ntime.sleep(60)", "timeoutMs": 500}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["state"], "timeout");
    // Fresh kernel: y is gone, and the restart is announced.
    let out = k.call(json!({"code": "print('y' in dir())"}), &ctx()).await.unwrap();
    assert_eq!(out["state"], "ok");
    assert_eq!(out["restarted"], true);
    assert_eq!(out["output"].as_str().unwrap().trim(), "False");
}

#[tokio::test]
async fn reset_discards_state_without_restart_notice_afterwards() {
    require_python!();
    let k = KernelTool::new();
    k.call(json!({"code": "z = 1"}), &ctx()).await.unwrap();
    let out = k.call(json!({"action": "reset"}), &ctx()).await.unwrap();
    assert_eq!(out["restarted"], true);
    // The reset itself announced the restart; the next exec is a plain
    // fresh start, not a surprise.
    let out = k.call(json!({"code": "print('z' in dir())"}), &ctx()).await.unwrap();
    assert_eq!(out["output"].as_str().unwrap().trim(), "False");
    assert!(out.get("restarted").is_none());
}

#[tokio::test]
async fn kernel_crash_is_detected_and_reported() {
    require_python!();
    let k = KernelTool::new();
    k.call(json!({"code": "import os"}), &ctx()).await.unwrap();
    // os._exit skips the driver loop entirely — the process just dies.
    let out = k.call(json!({"code": "os._exit(3)"}), &ctx()).await.unwrap();
    assert_eq!(out["state"], "timeout");
    let out = k.call(json!({"code": "print(1)"}), &ctx()).await.unwrap();
    assert_eq!(out["restarted"], true);
    assert_eq!(out["output"].as_str().unwrap().trim(), "1");
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_kernel_tool_kills_the_kernel() {
    require_python!();
    {
        let k = KernelTool::new();
        k.call(json!({"code": "marker_275_5 = 1"}), &ctx()).await.unwrap();
    } // dropped
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let found = std::process::Command::new("pgrep")
        .args(["-f", "ORCA_K_"])
        .output()
        .unwrap();
    assert!(!found.status.success(), "kernel process must die with the tool");
}
```

Note on `kernel_crash_is_detected_and_reported`: `os._exit` closes the driver's stdout, so the read loop sees EOF and lands in the same recovery path as a timeout (`state: "timeout"`). If running it shows a broken-pipe error path instead (the `ToolError` from the stdin write), adjust the assertion to accept either: the invariant under test is *the next call restarts fresh and says so*, not which of the two same-turn shapes surfaced.

Note on `dropping_kernel_tool_kills_the_kernel`: the pgrep pattern `ORCA_K_` matches the nonce argv of any live driver. Other kernel tests run concurrently by default — run this test file with `--test-threads=1` (see Step 2) so no sibling kernel is alive during the assert.

- [ ] **Step 2: Run them**

Run: `cargo test -p orca-harness-tools --test kernel -- --test-threads=1`
Expected: PASS. If `kernel_crash_is_detected_and_reported` fails on the same-turn shape, apply the note above and re-run.

- [ ] **Step 3: Commit**

```bash
git add crates/tools/tests/kernel.rs
git commit -m "test(tools): kernel recovery — timeout wedge, crash, reset, drop"
```

---

### Task 6: `subagent` tool — core

**Files:**
- Create: `crates/tools/src/subagent.rs`
- Modify: `crates/tools/src/lib.rs` (module + exports)
- Test: `crates/tools/tests/subagent.rs`

- [ ] **Step 1: Write the failing tests** — create `crates/tools/tests/subagent.rs`:

```rust
//! Subagent tool driven by the scripted model — no network, fully
//! deterministic.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    CancellationToken, Context, Limits, Model, ModelError, ModelResponse, Tool, ToolContext,
    ToolSchema, Usage,
};
use orca_harness_tools::{SubagentTool, Workspace};

static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn temp_ws() -> (Workspace, std::path::PathBuf) {
    let n = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("orca-harness-sub-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    (Workspace::new(dir.clone()), dir)
}

fn ctx() -> ToolContext {
    ToolContext {
        call_id: "t".into(),
        tool_name: "subagent".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

#[tokio::test]
async fn runs_task_and_reports_answer_and_usage() {
    let model = Arc::new(ScriptedModel::new(vec![ModelResponse::Final {
        text: "found 3 files".into(),
        usage: Some(Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        }),
    }]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model, &ws);
    let out = tool.call(json!({"task": "count files"}), &ctx()).await.unwrap();
    assert_eq!(out["answer"], "found 3 files");
    assert_eq!(out["usage"]["inputTokens"], 10);
    assert_eq!(out["usage"]["outputTokens"], 5);
}

#[tokio::test]
async fn inner_agent_executes_real_tools() {
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call(
            "1",
            "write_file",
            json!({"path": "note.txt", "content": "from the subagent"}),
        )],
        "wrote it",
    ));
    let (ws, dir) = temp_ws();
    let tool = SubagentTool::new(model, &ws);
    let out = tool.call(json!({"task": "write a note"}), &ctx()).await.unwrap();
    assert_eq!(out["answer"], "wrote it");
    assert_eq!(
        std::fs::read_to_string(dir.join("note.txt")).unwrap(),
        "from the subagent"
    );
}

/// A model that never answers — for cancellation tests.
struct StallModel;

#[async_trait]
impl Model for StallModel {
    async fn generate(
        &self,
        _context: &Context,
        _tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        tokio::time::sleep(Duration::from_secs(300)).await;
        Err(ModelError::InvalidResponse("unreachable".into()))
    }
}

#[tokio::test]
async fn parent_cancellation_reaches_the_inner_run() {
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(Arc::new(StallModel), &ws);
    let cancel = CancellationToken::new();
    let tctx = ToolContext {
        call_id: "t".into(),
        tool_name: "subagent".into(),
        cancellation: cancel.clone(),
        deadline: None,
    };
    let handle = tokio::spawn(async move { tool.call(json!({"task": "stall"}), &tctx).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("cancellation must end the inner run promptly")
        .unwrap();
    assert!(result.is_err());
}

#[tokio::test]
async fn step_limit_exhaustion_is_a_tool_error() {
    // One permitted step that returns tool calls: the run cannot finish.
    let model = Arc::new(ScriptedModel::new(vec![ModelResponse::tool_calls(vec![
        call("1", "list_dir", json!({"path": "."})),
    ])]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model, &ws).limits(Limits {
        max_steps: 1,
        ..Limits::default()
    });
    let result = tool.call(json!({"task": "loop forever"}), &ctx()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn task_is_required() {
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(Arc::new(StallModel), &ws);
    let err = tool.call(json!({}), &ctx()).await.unwrap_err();
    assert!(err.to_string().contains("task"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p orca-harness-tools --test subagent`
Expected: FAIL to compile — `SubagentTool` does not exist.

- [ ] **Step 3: Implement** — create `crates/tools/src/subagent.rs`:

```rust
//! `subagent` — spawn an independent in-process agent.
//!
//! Each call builds a fresh [`Agent`](orca_harness_core::Agent) — own
//! context, own loop, own tool set — runs one complete task, and returns
//! only the final answer plus token usage. Calls issued in the same batch
//! fan out in parallel, so one orchestrating model can farm work to N
//! workers at once.
//!
//! Nesting is the tool's own affair: a `SubagentTool` at depth `d` hands
//! its children a depth `d + 1` replica of itself only while
//! `d + 1 < max_depth`, where `max_depth` lives behind a shared
//! [`SubagentDepth`] handle a host can adjust mid-session (the CLI's
//! `/subagents` command). At the limit the child simply has no
//! `subagent` tool — no denials, no recursion.
//!
//! Lifetime: the per-call tool instances (including a fresh `process`
//! manager) drop when the call returns, so anything a subagent spawned
//! dies with it. Cancelling the parent run cancels every level below.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::{
    Agent, Concurrency, Context, Extension, ExtensionError, Limits, Model, ModelResponse,
    Subscriptions, Tool, ToolContext, ToolError, ToolSchema, Usage,
};

use crate::{core_tools, Workspace};

/// Bounds on subagent nesting depth. Each extra level multiplies model
/// calls, so the ceiling is deliberately low.
pub const MIN_SUBAGENT_DEPTH: u32 = 1;
pub const MAX_SUBAGENT_DEPTH: u32 = 5;

/// Shared, host-adjustable cap on subagent nesting. `1` (the default)
/// lets a top-level agent spawn workers that cannot nest further; read
/// at spawn time, so changes apply to the next spawn.
#[derive(Clone, Debug)]
pub struct SubagentDepth(Arc<AtomicU32>);

impl Default for SubagentDepth {
    fn default() -> Self {
        Self::new(MIN_SUBAGENT_DEPTH)
    }
}

impl SubagentDepth {
    pub fn new(max_depth: u32) -> Self {
        Self(Arc::new(AtomicU32::new(clamp_depth(max_depth))))
    }

    pub fn get(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }

    /// Set the cap, clamped to the permitted range; returns the value
    /// actually stored.
    pub fn set(&self, max_depth: u32) -> u32 {
        let clamped = clamp_depth(max_depth);
        self.0.store(clamped, Ordering::Relaxed);
        clamped
    }
}

fn clamp_depth(depth: u32) -> u32 {
    depth.clamp(MIN_SUBAGENT_DEPTH, MAX_SUBAGENT_DEPTH)
}

type ToolFactory = Arc<dyn Fn() -> Vec<Arc<dyn Tool>> + Send + Sync>;

pub struct SubagentTool<M: Model + Clone + 'static> {
    model: M,
    tools: ToolFactory,
    limits: Limits,
    system_prompt: Option<String>,
    /// Distance from the top-level agent; the instance registered there
    /// is depth 0.
    depth: u32,
    max_depth: SubagentDepth,
}

impl<M: Model + Clone + 'static> SubagentTool<M> {
    /// Subagents equipped with [`core_tools`] rooted at `ws`.
    pub fn new(model: M, ws: &Workspace) -> Self {
        let ws = ws.clone();
        Self::with_tools(model, Arc::new(move || core_tools(&ws)))
    }

    /// Subagents equipped with an arbitrary tool set. The factory should
    /// NOT include a `subagent` tool — nesting is added by this tool
    /// itself, governed by [`SubagentDepth`].
    pub fn with_tools(model: M, tools: ToolFactory) -> Self {
        Self {
            model,
            tools,
            // Deliberately below Limits::default() (32): workers get a
            // tighter leash than the orchestrator.
            limits: Limits {
                max_steps: 24,
                ..Limits::default()
            },
            system_prompt: None,
            depth: 0,
            max_depth: SubagentDepth::default(),
        }
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Default system prompt for spawned agents (a call's `systemPrompt`
    /// overrides it).
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Share a depth handle (and thus a runtime-adjustable nesting cap).
    pub fn max_depth(mut self, depth: SubagentDepth) -> Self {
        self.max_depth = depth;
        self
    }

    fn child_replica(&self) -> Self {
        Self {
            model: self.model.clone(),
            tools: self.tools.clone(),
            limits: self.limits.clone(),
            system_prompt: self.system_prompt.clone(),
            depth: self.depth + 1,
            max_depth: self.max_depth.clone(),
        }
    }
}

/// Minimal usage accumulator for the inner agent. Local to this module:
/// pulling in the extensions crate for one hook would invert the crate
/// layering.
#[derive(Clone, Default)]
struct Meter(Arc<StdMutex<Usage>>);

impl Meter {
    fn total(&self) -> Usage {
        *self.0.lock().unwrap()
    }
}

#[async_trait]
impl Extension for Meter {
    fn name(&self) -> &str {
        "subagent_usage"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().after_model()
    }

    async fn after_model(
        &self,
        _context: &mut Context,
        response: &ModelResponse,
    ) -> Result<(), ExtensionError> {
        if let Some(usage) = response.usage() {
            self.0.lock().unwrap().add(usage);
        }
        Ok(())
    }
}

#[async_trait]
impl<M: Model + Clone + 'static> Tool for SubagentTool<M> {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "subagent".into(),
            description: "Spawn an independent agent with its own context and full file/shell \
                tool access to work on one task. Give it a complete, self-contained task \
                description — it sees nothing of this conversation and returns only its final \
                answer. Several subagent calls issued in the same response run in parallel."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "Complete, self-contained task for the agent."},
                    "systemPrompt": {"type": "string", "description": "Optional system prompt override for this agent."}
                },
                "required": ["task"]
            }),
        }
    }

    fn concurrency(&self, _input: &Value) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let task = input
            .get("task")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`task` (string) is required"))?;
        let system = input
            .get("systemPrompt")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.system_prompt.clone());

        let meter = Meter::default();
        let usage = meter.clone();
        let mut agent = Agent::new(self.model.clone())
            .limits(self.limits.clone())
            .extension(meter);
        if let Some(system) = system {
            agent = agent.system_prompt(system);
        }
        for tool in (self.tools)() {
            agent = agent.tool_arc(tool);
        }
        if self.depth + 1 < self.max_depth.get() {
            agent = agent.tool_arc(Arc::new(self.child_replica()));
        }

        let answer = agent
            .run_with_cancellation(task, ctx.cancellation.child_token())
            .await
            .map_err(|e| ToolError::msg(format!("subagent failed: {e}")))?;
        let total = usage.total();
        Ok(json!({
            "answer": answer,
            "usage": {
                "inputTokens": total.input_tokens,
                "outputTokens": total.output_tokens,
            }
        }))
    }
}
```

Register in `crates/tools/src/lib.rs`:

```rust
mod subagent;
```
and
```rust
pub use subagent::{SubagentDepth, SubagentTool, MAX_SUBAGENT_DEPTH, MIN_SUBAGENT_DEPTH};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p orca-harness-tools --test subagent`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/tools/src/subagent.rs crates/tools/src/lib.rs crates/tools/tests/subagent.rs
git commit -m "feat(tools): subagent tool — in-process agent fan-out"
```

---

### Task 7: `subagent` nesting behavior

**Files:**
- Test: `crates/tools/tests/subagent.rs`
- Modify: `crates/tools/src/subagent.rs` (only if a test exposes a bug)

- [ ] **Step 1: Write the tests** — append to `crates/tools/tests/subagent.rs`:

```rust
use orca_harness_core::Message;
use orca_harness_tools::SubagentDepth;

#[tokio::test]
async fn depth_two_lets_a_subagent_spawn_a_grandchild() {
    // One shared script, consumed strictly in order:
    //   child agent step 1 -> calls subagent
    //   grandchild agent   -> final
    //   child agent step 2 -> final
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call("1", "subagent", json!({"task": "inner"}))]),
        ModelResponse::final_text("grandchild done"),
        ModelResponse::final_text("child done"),
    ]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model.clone(), &ws).max_depth(SubagentDepth::new(2));
    let out = tool.call(json!({"task": "outer"}), &ctx()).await.unwrap();
    assert_eq!(out["answer"], "child done");
    assert_eq!(model.generate_calls(), 3, "grandchild must actually have run");
}

#[tokio::test]
async fn default_depth_gives_children_no_subagent_tool() {
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call("1", "subagent", json!({"task": "inner"}))]),
        ModelResponse::final_text("child done"),
    ]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model.clone(), &ws); // max_depth defaults to 1
    let out = tool.call(json!({"task": "outer"}), &ctx()).await.unwrap();
    assert_eq!(out["answer"], "child done");
    // The child's subagent call must have come back as an unknown tool.
    let contexts = model.observed_contexts();
    let last = contexts.last().unwrap();
    let saw_unknown = last.messages().iter().any(|m| match m {
        Message::Tool { results } => results
            .iter()
            .any(|r| r.is_error && r.output.to_string().contains("unknown tool")),
        _ => false,
    });
    assert!(saw_unknown, "child had no subagent tool, call must error");
}

#[tokio::test]
async fn raising_the_shared_depth_applies_to_the_next_spawn() {
    let depth = SubagentDepth::new(1);
    let model = Arc::new(ScriptedModel::new(vec![
        // First call (depth 1): nested attempt fails as unknown tool.
        ModelResponse::tool_calls(vec![call("1", "subagent", json!({"task": "inner"}))]),
        ModelResponse::final_text("first done"),
        // Second call (after raise to 2): nesting works.
        ModelResponse::tool_calls(vec![call("2", "subagent", json!({"task": "inner"}))]),
        ModelResponse::final_text("grandchild done"),
        ModelResponse::final_text("second done"),
    ]));
    let (ws, _dir) = temp_ws();
    let tool = SubagentTool::new(model.clone(), &ws).max_depth(depth.clone());

    let out = tool.call(json!({"task": "one"}), &ctx()).await.unwrap();
    assert_eq!(out["answer"], "first done");

    assert_eq!(depth.set(2), 2);
    let out = tool.call(json!({"task": "two"}), &ctx()).await.unwrap();
    assert_eq!(out["answer"], "second done");
    assert_eq!(model.generate_calls(), 5);
}

#[test]
fn depth_handle_clamps_to_permitted_range() {
    let depth = SubagentDepth::new(0);
    assert_eq!(depth.get(), 1);
    assert_eq!(depth.set(99), 5);
    assert_eq!(depth.get(), 5);
    assert_eq!(depth.set(3), 3);
}
```

- [ ] **Step 2: Run them**

Run: `cargo test -p orca-harness-tools --test subagent`
Expected: PASS (the Task 6 implementation already carries the logic; failures here mean the replica/depth arithmetic is wrong — fix `subagent.rs`, re-run).

- [ ] **Step 3: Commit**

```bash
git add crates/tools/tests/subagent.rs
git commit -m "test(tools): subagent nesting — depth gating and runtime raise"
```

---

### Task 8: CLI wiring — registration, gating, system prompt, env

**Files:**
- Modify: `crates/cli/src/main.rs` (Config, parse_args, system_prompt, build_agent, run_mode)
- Modify: `crates/cli/src/headless.rs`
- Modify: `crates/cli/src/approval.rs` (GATED_TOOLS)

- [ ] **Step 1: Write the failing tests**

In `crates/cli/src/approval.rs`, append to the existing `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn kernel_and_subagent_are_gated() {
        assert!(GATED_TOOLS.contains(&"kernel"));
        assert!(GATED_TOOLS.contains(&"subagent"));
    }
```

In `crates/cli/src/main.rs`, extend the existing `main_tests` module:

```rust
    #[test]
    fn system_prompt_advertises_kernel_and_subagent() {
        let prompt = system_prompt(&Workspace::new(PathBuf::from(".")), false);
        assert!(prompt.contains("kernel"));
        assert!(prompt.contains("subagent"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p orca-cli kernel_and_subagent_are_gated system_prompt_advertises_kernel_and_subagent`
Expected: both FAIL.

- [ ] **Step 3: Implement the wiring**

`crates/cli/src/approval.rs` — extend the gated list (kernel executes arbitrary code, same class as shell; subagent spawns an agent whose inner tools run un-gated, so the spawn itself is the approval point):

```rust
pub const GATED_TOOLS: &[&str] = &[
    "shell",
    "write_file",
    "edit_file",
    "web_fetch",
    "kernel",
    "subagent",
];
```

`crates/cli/src/main.rs`:

1. Config field and USAGE line. Add to `Config`:

```rust
    pub subagent_depth: u32,
```

In `USAGE`, after the `--max-steps` line:

```text
  --theme NAME       mono (default) or color
```
becomes (insert one line above `--theme`):
```text
  --subagent-depth N subagent nesting levels, 1-5 (env ORCA_SUBAGENT_DEPTH;
                     default 1; /subagents adjusts it live in the TUI)
  --theme NAME       mono (default) or color
```

2. In `parse_args`, initialize and parse:

```rust
    let mut subagent_depth: u32 = std::env::var("ORCA_SUBAGENT_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
```

Add the flag arm next to `--max-steps`:

```rust
            "--subagent-depth" => {
                subagent_depth = value("--subagent-depth")?
                    .parse()
                    .map_err(|_| "--subagent-depth expects a number".to_string())?
            }
```

Add `subagent_depth,` to the `Ok(Config { ... })` literal.

3. Imports:

```rust
use orca_harness_tools::{core_tools, KernelTool, SubagentDepth, SubagentTool, Workspace};
```

4. `system_prompt` — in the tool enumeration sentence, change

```text
         You act through tools: shell, process (persistent sessions and background \
         processes), read_file, write_file, edit_file, list_dir, grep, glob, \
         read_tool_result (re-read the full output of a truncated result){web_tools}. \
```
to
```text
         You act through tools: shell, process (persistent sessions and background \
         processes), kernel (persistent Python — variables survive across calls; \
         print what you need to see), subagent (spawn an independent agent with its \
         own context and tools for a self-contained task; parallel calls fan out), \
         read_file, write_file, edit_file, list_dir, grep, glob, \
         read_tool_result (re-read the full output of a truncated result){web_tools}. \
```

5. `build_agent` — require `Clone` and register both tools. Change the signature and add registrations after the `core_tools` loop:

```rust
fn build_agent<M: Model + Clone + 'static>(
    model: M,
    cfg: &Config,
    ws: &Workspace,
    ui: &mpsc::UnboundedSender<UiMsg>,
    subagent_depth: &SubagentDepth,
) -> Agent<M> {
```

and before the final `agent`:

```rust
    let root = ws.root().to_string_lossy().into_owned();
    agent = agent.tool_arc(std::sync::Arc::new(KernelTool::new().working_dir(root)));
    agent = agent.tool_arc(std::sync::Arc::new(
        SubagentTool::new(model_for_subagents, ws).max_depth(subagent_depth.clone()),
    ));
    agent
```

`model_for_subagents`: `Agent::new(model)` consumes `model`, so clone first at the top of `build_agent`:

```rust
    let model_for_subagents = model.clone();
```

6. `run_mode` — create the shared handle and thread it through:

```rust
    let subagent_depth = SubagentDepth::new(cfg.subagent_depth);
```

(immediately after `let endpoint = ...`), pass it into the build closure:

```rust
    let build = {
        let cfg = cfg.clone();
        let ui_tx = ui_tx.clone();
        let subagent_depth = subagent_depth.clone();
        move |endpoint: &Endpoint| {
            let ws = Workspace::new(&cfg.workspace);
            build_agent(endpoint.build_model(), &cfg, &ws, &ui_tx, &subagent_depth)
        }
    };
```

and into `TuiConfig` (field added in Task 9):

```rust
    let tui_cfg = tui::TuiConfig {
        model_name: cfg.model.clone(),
        workspace_name: cfg.workspace.display().to_string(),
        subagent_depth: subagent_depth.clone(),
    };
```

Note: `run_mode` compiles only after Task 9 adds the `TuiConfig` field. To keep this task self-contained and green, add the field to `TuiConfig` now as part of this task (declaration only — the `/subagents` command that uses it is Task 9):

```rust
pub struct TuiConfig {
    pub model_name: String,
    pub workspace_name: String,
    pub subagent_depth: orca_harness_tools::SubagentDepth,
}
```

(check `tui.rs` for other `TuiConfig` literals — `welcome_lines` tests or fixtures may construct one; update every construction site the compiler flags.)

7. `headless.rs` — same registration. Change the signature and add after its `core_tools` loop:

```rust
pub async fn run<M: Model + Clone + 'static>(
    cfg: &Config,
    model: M,
    ws: &Workspace,
    system_prompt: &str,
) -> i32 {
    let model_for_subagents = model.clone();
```

and after the `for tool in core_tools(ws)` loop:

```rust
    let root = ws.root().to_string_lossy().into_owned();
    agent = agent.tool_arc(Arc::new(KernelTool::new().working_dir(root)));
    agent = agent.tool_arc(Arc::new(
        SubagentTool::new(model_for_subagents, ws)
            .max_depth(SubagentDepth::new(cfg.subagent_depth)),
    ));
```

with imports:

```rust
use orca_harness_tools::{core_tools, KernelTool, SubagentDepth, SubagentTool, Workspace};
```

- [ ] **Step 4: Run the tests and build**

Run: `cargo test -p orca-cli && cargo build -p orca-cli`
Expected: PASS / builds clean.

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/main.rs crates/cli/src/headless.rs crates/cli/src/approval.rs crates/cli/src/tui.rs
git commit -m "feat(cli): register kernel and subagent tools, gate them behind approval"
```

---

### Task 9: TUI `/subagents` command

**Files:**
- Modify: `crates/cli/src/commands.rs` (registry entry)
- Modify: `crates/cli/src/tui.rs` (`slash_command`, `/help` text, tests)

- [ ] **Step 1: Write the failing test** — `tui.rs` has no test module; add one at the bottom:

```rust
#[cfg(test)]
mod subagents_command_tests {
    use super::*;
    use orca_harness_tools::SubagentDepth;

    fn test_app(depth: SubagentDepth) -> App {
        App::new(TuiConfig {
            model_name: "m".into(),
            workspace_name: "w".into(),
            subagent_depth: depth,
        })
    }

    #[tokio::test]
    async fn subagents_command_sets_and_clamps_depth() {
        let depth = SubagentDepth::new(1);
        let mut app = test_app(depth.clone());
        let (worker, _rx) = tokio::sync::mpsc::unbounded_channel();

        slash_command(&mut app, "subagents 3", &worker, 80);
        assert_eq!(depth.get(), 3);

        slash_command(&mut app, "subagents 99", &worker, 80);
        assert_eq!(depth.get(), 5, "out-of-range input clamps");

        // Bare form only reports; it must not change the value.
        slash_command(&mut app, "subagents", &worker, 80);
        assert_eq!(depth.get(), 5);

        // Garbage input leaves the value alone.
        slash_command(&mut app, "subagents lots", &worker, 80);
        assert_eq!(depth.get(), 5);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p orca-cli subagents_command_sets_and_clamps_depth`
Expected: FAIL — the command falls through to `unknown command`, depth stays 1.

- [ ] **Step 3: Implement**

`crates/cli/src/commands.rs` — add to `COMMANDS` (after the `provider` entry):

```rust
    CommandSpec {
        name: "subagents",
        description: "show or set subagent nesting depth (1-5)",
        category: "Session",
        takes_args: true,
    },
```

`crates/cli/src/tui.rs` — in `slash_command`, before the bare `match command` block (alongside the other prefix handlers):

```rust
    if let Some(rest) = command.strip_prefix("subagents") {
        if rest.is_empty() {
            app.push_line(Line::from(Span::styled(
                format!("subagent nesting depth: {}", app.cfg.subagent_depth.get()),
                dim,
            )));
            return;
        }
        if let Some(arg) = rest.strip_prefix(' ') {
            match arg.trim().parse::<u32>() {
                Ok(depth) => {
                    let set = app.cfg.subagent_depth.set(depth);
                    app.push_line(Line::from(Span::styled(
                        format!("subagent nesting depth set to {set} (applies to the next spawn)"),
                        dim,
                    )));
                }
                Err(_) => {
                    app.push_line(Line::from(Span::styled(
                        "usage: /subagents [1-5]",
                        theme().error,
                    )));
                }
            }
            return;
        }
    }
```

In the `/help` listing, add after the `/provider` line:

```rust
                "/subagents [n] show or set subagent nesting depth (1-5)",
```

- [ ] **Step 4: Run**

Run: `cargo test -p orca-cli`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/commands.rs crates/cli/src/tui.rs
git commit -m "feat(cli): /subagents command adjusts nesting depth live"
```

---

### Task 10: CLI signal handling — clean exits on SIGTERM/SIGHUP

Moving children into their own process groups (Task 1) removed the accidental SIGHUP they used to share with the CLI's terminal. This task replaces that accident with deliberate cleanup: signals route through the normal quit path so destructors (and their synchronous group kills) run.

**Files:**
- Modify: `crates/cli/src/main.rs` (shared `shutdown_signal` helper)
- Modify: `crates/cli/src/tui.rs` (select arm)
- Modify: `crates/cli/src/headless.rs` (extend the existing ctrl_c task)

- [ ] **Step 1: Add the helper** — in `crates/cli/src/main.rs` (near `repair_dangling_tool_calls`):

```rust
/// Resolves when the process receives SIGTERM or SIGHUP (terminal window
/// closed). Children now live in their own process groups, so the CLI
/// must exit its run loop cleanly for the Drop-time group kills to fire.
#[cfg(unix)]
pub(crate) async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = hup.recv() => {}
    }
}

#[cfg(not(unix))]
pub(crate) async fn shutdown_signal() {
    std::future::pending::<()>().await
}
```

- [ ] **Step 2: TUI** — in `tui::run`, before the `while !app.quit` loop:

```rust
    let shutdown = crate::shutdown_signal();
    tokio::pin!(shutdown);
```

and a new select arm (the `if !app.quit` guard keeps the completed future from being polled again):

```rust
            _ = &mut shutdown, if !app.quit => {
                app.quit = true;
            }
```

- [ ] **Step 3: Headless** — in `headless::run`, replace the ctrl_c task body:

```rust
    let cancel = CancellationToken::new();
    let cancel_on_signal = cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if result.is_err() {
                    return;
                }
            }
            _ = crate::shutdown_signal() => {}
        }
        cancel_on_signal.cancel();
    });
```

- [ ] **Step 4: Build, run the suite, and verify by hand**

Run: `cargo build -p orca-cli && cargo test -p orca-cli`
Expected: builds clean, tests pass.

Manual verification (the signal path is not unit-testable from inside the process):

```bash
cargo build --release -p orca-cli
./target/release/orca -p "use the process tool to spawn 'sleep 271.3 & wait', then reply done" --auto-approve &
sleep 8 && kill -TERM %1
sleep 2 && pgrep -f "sleep 271.3" || echo "clean: no dangling children"
```

Expected final line: `clean: no dangling children`. (Requires a reachable model endpoint; if none is available, note that and rely on the tools-crate drop tests, which cover the same kill mechanics.)

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/main.rs crates/cli/src/tui.rs crates/cli/src/headless.rs
git commit -m "feat(cli): SIGTERM/SIGHUP exit cleanly so child process groups die"
```

---

### Task 11: Documentation and final verification

**Files:**
- Modify: `crates/tools/src/lib.rs` (crate docs)
- Modify: `README.md` (layout + interactive-mode notes)
- Modify: `docs/superpowers/specs/2026-08-20-kernel-subagent-tools-design.md` (one param rename)

- [ ] **Step 1: Crate docs** — in `crates/tools/src/lib.rs`, extend the opening doc paragraph:

```rust
//! The core set of tools that make the harness independently useful: run
//! commands on the host (or a target machine / container), keep
//! long-lived processes and interactive sessions alive across calls, and
//! read, write, edit, list, glob, and search files. Two workflow tools
//! build on the same kernel primitives: [`KernelTool`] (persistent
//! Python compute — state survives across calls) and [`SubagentTool`]
//! (spawn independent in-process agents, with nesting governed by a
//! shared [`SubagentDepth`]). A separate [`fs_admin_tools`] bundle adds
//! copy/rename/delete/mkdir/stat for shell-less restricted agents.
```

(keep the rest of the existing paragraph and example intact).

- [ ] **Step 2: README** — in the layout block, extend the `tools/` line:

```text
│   ├── tools/            # core host/target tools: shell, process
│   │                     # (persistent sessions / background processes),
│   │                     # kernel (persistent Python compute), subagent
│   │                     # (in-process agent fan-out with adjustable
│   │                     # nesting), read/write/edit/list files, grep,
│   │                     # glob — the set that makes an agent
│   │                     # independently useful; plus an opt-in fs-admin
│   │                     # bundle (copy/rename/delete/mkdir/stat)
```

In the interactive-mode paragraph, extend the gated-tools sentence:

```text
Gated tools (`shell`, `write_file`, `edit_file`, `kernel`, `subagent`) pause
behind a y/a/n approval prompt
```

- [ ] **Step 3: Spec consistency** — in the spec's subagent schema table, rename `system_prompt` to `systemPrompt` (same camelCase rationale the spec already applies to `timeoutMs`).

- [ ] **Step 4: Full workspace verification**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets 2>&1 | tail -20`
Expected: all tests pass; no new clippy warnings introduced by these changes.

- [ ] **Step 5: Commit**

```bash
git add crates/tools/src/lib.rs README.md docs/superpowers/specs/2026-08-20-kernel-subagent-tools-design.md
git commit -m "docs: kernel and subagent tools in crate docs and README"
```

---

## Self-Review Notes

- **Spec coverage:** kernel driver/framing/sentinel (Task 4), timeout-wedge-respawn + reset + stdin-EOF backstop (Tasks 4-5), subagent in-process + usage + cancellation + limits (Task 6), nesting + shared depth handle (Task 7), CLI registration + gating + env/flag (Task 8), `/subagents` (Task 9), process groups + group kills + sync Drop + signals (Tasks 1-3, 10), tests per spec section 5 (throughout), docs (Task 11). The spec's "kill action kills grandchildren" test is Task 3; the shell-side variant is Task 2.
- **Deviation from spec, deliberate:** the spec names `--subagent-depth` only as env `ORCA_SUBAGENT_DEPTH`; the plan also adds the flag since every other env knob in `parse_args` has one — same pattern, no new surface kind.
- **Type consistency:** `SubagentDepth::{new, get, set}` used identically in Tasks 6-9; `KernelTool::{new, working_dir}` in Tasks 4, 8; `pgroup::kill_group(u32)` in Tasks 1-4.
