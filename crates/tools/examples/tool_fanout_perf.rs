//! Performance probe for REAL core-tool calls under concurrent dispatch.
//!
//! Unlike the kernel's `fanout_probe` (no-op tools, measures pure
//! dispatch overhead), this drives the actual `write_file`, `read_file`,
//! and `shell` tools through the real Dispatcher and measures:
//!
//!   - fan-out latency (T0 dispatch entry → T2 last tool body entered),
//!     the harness overhead added on top of the tools' own work;
//!   - total wall clock for the batch, vs. the serial-sum lower bound,
//!     yielding an effective speedup;
//!   - throughput (tool calls per second).
//!
//! Run with:  cargo run -p orca-harness-tools --release --example tool_fanout_perf
//! Optional args: <batch-size> <iterations>

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use orca_harness_core::testing::call;
use orca_harness_core::{
    CancellationToken, Dispatcher, ExtensionRegistry, Tool, ToolCall, ToolContext, ToolError,
    ToolRegistry, ToolSchema,
};
use orca_harness_tools::{
    core_tools_with_guard, fs_admin_tools, FileGuard, ReadFileTool, ShellTool, Workspace,
    WriteFileTool,
};

/// [`Timed`] for already-boxed tools, so a whole registry can be wrapped.
struct TimedDyn {
    inner: Arc<dyn Tool>,
    epoch: Instant,
    first: Arc<AtomicU64>,
    last: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl Tool for TimedDyn {
    fn schema(&self) -> ToolSchema {
        self.inner.schema()
    }
    fn concurrency(&self, input: &Value) -> orca_harness_core::Concurrency {
        self.inner.concurrency(input)
    }
    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let now = self.epoch.elapsed().as_nanos() as u64;
        self.first.fetch_min(now, Ordering::Relaxed);
        self.last.fetch_max(now, Ordering::Relaxed);
        self.inner.call(input, ctx).await
    }
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() as f64 - 1.0) * p) as usize]
}

/// Wraps a real tool and stamps, into a shared epoch clock, the moment its
/// body starts — so we can measure T2 (last body entered) across a batch.
struct Timed<T: Tool> {
    inner: T,
    epoch: Instant,
    first: Arc<AtomicU64>,
    last: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl<T: Tool> Tool for Timed<T> {
    fn schema(&self) -> ToolSchema {
        self.inner.schema()
    }
    fn concurrency(&self, input: &Value) -> orca_harness_core::Concurrency {
        self.inner.concurrency(input)
    }
    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let now = self.epoch.elapsed().as_nanos() as u64;
        self.first.fetch_min(now, Ordering::Relaxed);
        self.last.fetch_max(now, Ordering::Relaxed);
        self.inner.call(input, ctx).await
    }
}

async fn measure(
    label: &str,
    n: usize,
    iters: usize,
    make_calls: impl Fn(usize) -> Vec<ToolCall>,
    build_registry: impl Fn(Instant, Arc<AtomicU64>, Arc<AtomicU64>) -> ToolRegistry,
    per_call_work_ns: u64,
) {
    let dispatcher = Dispatcher::new();
    let extensions = ExtensionRegistry::new();
    let epoch = Instant::now();

    let mut fanout = Vec::with_capacity(iters);
    let mut wall = Vec::with_capacity(iters);

    for _ in 0..iters {
        let first = Arc::new(AtomicU64::new(u64::MAX));
        let last = Arc::new(AtomicU64::new(0));
        let tools = build_registry(epoch, first.clone(), last.clone());
        let batch = make_calls(n);

        let t0 = epoch.elapsed().as_nanos() as u64;
        dispatcher
            .execute(
                batch,
                &tools,
                &extensions,
                &CancellationToken::new(),
                None,
                n,
            )
            .await
            .unwrap();
        let t_end = epoch.elapsed().as_nanos() as u64;

        fanout.push(last.load(Ordering::SeqCst).saturating_sub(t0));
        wall.push(t_end - t0);
    }

    fanout.sort_unstable();
    wall.sort_unstable();

    let wall_p50 = pct(&wall, 0.50);
    let serial_lower_bound = per_call_work_ns * n as u64;
    let speedup = serial_lower_bound as f64 / wall_p50 as f64;
    let throughput = n as f64 / (wall_p50 as f64 / 1e9);

    println!("── {label}  (n={n}) ──");
    println!(
        "   fan-out overhead (T2-T0)  p50={:>8.1}µs  p99={:>8.1}µs",
        pct(&fanout, 0.50) as f64 / 1e3,
        pct(&fanout, 0.99) as f64 / 1e3,
    );
    println!(
        "   total wall clock          p50={:>8.1}µs  p99={:>8.1}µs",
        wall_p50 as f64 / 1e3,
        pct(&wall, 0.99) as f64 / 1e3,
    );
    if per_call_work_ns > 0 {
        println!(
            "   serial lower bound {:>6.1}ms → effective speedup {:>5.1}×",
            serial_lower_bound as f64 / 1e6,
            speedup,
        );
    }
    println!("   throughput               {throughput:>10.0} tool calls/sec\n");
}

/// A shell tool whose command sleeps a fixed time, to model real tool
/// latency (subprocess + I/O) without depending on machine speed.
fn timed_shell(epoch: Instant, first: Arc<AtomicU64>, last: Arc<AtomicU64>) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Timed {
        inner: ShellTool::local(),
        epoch,
        first,
        last,
    }));
    tools
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100);
    let iters: usize = std::env::args()
        .nth(2)
        .and_then(|a| a.parse().ok())
        .unwrap_or(50);

    println!(
        "\nReal core-tool fan-out perf — {} worker threads, {iters} iterations/case\n",
        std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(0),
    );

    // Case 1: file writes to distinct paths (real filesystem work).
    let dir = std::env::temp_dir().join(format!("orca-perf-write-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ws = Workspace::new(dir.clone());
    {
        let ws = ws.clone();
        measure(
            "write_file → distinct paths",
            n,
            iters,
            |n| {
                (0..n)
                    .map(|i| {
                        call(
                            &format!("c{i}"),
                            "write_file",
                            json!({"path": format!("f{i}.txt"), "content": "x".repeat(256)}),
                        )
                    })
                    .collect()
            },
            move |epoch, first, last| {
                let mut tools = ToolRegistry::new();
                tools.register(Arc::new(Timed {
                    inner: WriteFileTool::new(ws.clone()),
                    epoch,
                    first,
                    last,
                }));
                tools
            },
            0,
        )
        .await;
    }

    // Case 2: file reads (seed a file, read it N times concurrently).
    std::fs::write(dir.join("seed.txt"), "y".repeat(4096)).unwrap();
    {
        let ws = ws.clone();
        measure(
            "read_file → same file",
            n,
            iters,
            |n| {
                (0..n)
                    .map(|i| call(&format!("c{i}"), "read_file", json!({"path": "seed.txt"})))
                    .collect()
            },
            move |epoch, first, last| {
                let mut tools = ToolRegistry::new();
                tools.register(Arc::new(Timed {
                    inner: ReadFileTool::new(ws.clone()),
                    epoch,
                    first,
                    last,
                }));
                tools
            },
            0,
        )
        .await;
    }

    // Case 3: real subprocesses that each sleep 20ms — models genuine tool
    // latency, so the speedup number is meaningful.
    let sleep_ns = 20_000_000u64;
    measure(
        "shell → 20ms subprocess each",
        n.min(64),
        iters.min(20),
        |n| {
            (0..n)
                .map(|i| call(&format!("c{i}"), "shell", json!({"command": "sleep 0.02"})))
                .collect::<Vec<_>>()
                .into_iter()
                .take(n)
                .collect()
        },
        timed_shell,
        sleep_ns,
    )
    .await;

    // Case 4: one mixed batch across the whole shipped tool set (core +
    // fs admin), every call repeatable across iterations.
    let mixed_dir = std::env::temp_dir().join(format!("orca-perf-mixed-{}", std::process::id()));
    std::fs::create_dir_all(mixed_dir.join("src")).unwrap();
    std::fs::write(mixed_dir.join("src/seed.rs"), "needle in seed\n").unwrap();
    std::fs::write(mixed_dir.join("copy_me.txt"), "payload\n").unwrap();
    let mixed_ws = Workspace::new(mixed_dir.clone());
    // The registry is rebuilt every iteration (fresh timing counters),
    // but the read-before-write guard is not: one guard across the run
    // models one agent session, so the repeated `out.txt` write is a
    // real overwrite of a file these tools wrote, not a refusal.
    let mixed_guard = FileGuard::new();
    measure(
        "mixed batch → every tool at once",
        10,
        iters.min(20),
        |_| {
            vec![
                call("m-shell", "shell", json!({"command": "echo hi"})),
                call(
                    "m-proc",
                    "process",
                    json!({"action": "spawn", "command": "true"}),
                ),
                call("m-read", "read_file", json!({"path": "src/seed.rs"})),
                call(
                    "m-write",
                    "write_file",
                    json!({"path": "out.txt", "content": "x"}),
                ),
                call("m-list", "list_dir", json!({"path": "."})),
                call("m-grep", "grep", json!({"query": "needle"})),
                call("m-glob", "glob", json!({"pattern": "*.rs"})),
                call(
                    "m-copy",
                    "copy_file",
                    json!({"from": "copy_me.txt", "to": "copied.txt"}),
                ),
                call("m-mkdir", "create_folder", json!({"path": "made"})),
                call("m-info", "file_info", json!({"path": "copy_me.txt"})),
            ]
        },
        {
            let ws = mixed_ws.clone();
            let guard = mixed_guard.clone();
            move |epoch, first, last| {
                let mut tools = ToolRegistry::new();
                for tool in core_tools_with_guard(&ws, &guard)
                    .into_iter()
                    .chain(fs_admin_tools(&ws))
                {
                    tools.register(Arc::new(TimedDyn {
                        inner: tool,
                        epoch,
                        first: first.clone(),
                        last: last.clone(),
                    }));
                }
                tools
            }
        },
        0,
    )
    .await;

    std::fs::remove_dir_all(&mixed_dir).ok();
    std::fs::remove_dir_all(&dir).ok();
}
