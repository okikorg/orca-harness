//! Fan-out latency probe: measures dispatch latency (T1-T0) and fan-out
//! latency (T2-T0) where T0 = dispatch entry, T1 = first tool body
//! started, T2 = last tool body started. Run with --release.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use orca_harness_core::testing::call;
use orca_harness_core::{CancellationToken, Dispatcher, ExtensionRegistry, FnTool, ToolRegistry};

fn pct(sorted: &[u64], p: f64) -> u64 {
    sorted[((sorted.len() as f64 - 1.0) * p) as usize]
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
        .unwrap_or(200);

    // Epoch for cheap monotonic nanos shared across tasks.
    let epoch = Instant::now();
    let first_start = Arc::new(AtomicU64::new(u64::MAX));
    let last_start = Arc::new(AtomicU64::new(0));

    let noop = {
        let first = first_start.clone();
        let last = last_start.clone();
        FnTool::new("noop", "noop", json!({"type":"object"}), move |_i, _c| {
            let now = epoch.elapsed().as_nanos() as u64;
            first.fetch_min(now, Ordering::Relaxed);
            last.fetch_max(now, Ordering::Relaxed);
            async move { Ok(Value::Null) }
        })
    };

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(noop));
    let extensions = ExtensionRegistry::new();
    let dispatcher = Dispatcher::new();

    let mut dispatch_lat = Vec::with_capacity(iters);
    let mut fanout_lat = Vec::with_capacity(iters);
    let mut total_lat = Vec::with_capacity(iters);

    for _ in 0..iters {
        let batch: Vec<_> = (0..n)
            .map(|i| call(&format!("c{i}"), "noop", json!({})))
            .collect();
        first_start.store(u64::MAX, Ordering::SeqCst);
        last_start.store(0, Ordering::SeqCst);

        let t0 = epoch.elapsed().as_nanos() as u64;
        dispatcher
            .execute(
                batch,
                &tools,
                &extensions,
                &CancellationToken::new(),
                None,
                n.max(16),
            )
            .await
            .unwrap();
        let t3 = epoch.elapsed().as_nanos() as u64;

        dispatch_lat.push(first_start.load(Ordering::SeqCst).saturating_sub(t0));
        fanout_lat.push(last_start.load(Ordering::SeqCst).saturating_sub(t0));
        total_lat.push(t3 - t0);
    }

    for (name, v) in [
        ("dispatch (T1-T0)", &mut dispatch_lat),
        ("fan-out (T2-T0)", &mut fanout_lat),
        ("total round-trip", &mut total_lat),
    ] {
        v.sort_unstable();
        println!(
            "{name:18} n={n:4}  p50={:>8.1}µs  p90={:>8.1}µs  p99={:>8.1}µs  max={:>8.1}µs",
            pct(v, 0.50) as f64 / 1e3,
            pct(v, 0.90) as f64 / 1e3,
            pct(v, 0.99) as f64 / 1e3,
            v[v.len() - 1] as f64 / 1e3,
        );
    }
}
