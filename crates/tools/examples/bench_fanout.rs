//! Slow-tool fan-out at the unbounded default width vs bounded waves.
//!
//! Each shell call is a real subprocess sleeping 20 ms, so wall clock
//! directly shows how many waves the batch was split into.
//!
//! Run with:  cargo run -p orca-harness-tools --release --example bench_fanout
//! Optional arg: <call-count> (default 100)

use std::sync::Arc;
use std::time::Instant;

use serde_json::json;

use orca_harness_core::testing::call;
use orca_harness_core::{CancellationToken, Dispatcher, ExtensionRegistry, Limits, ToolRegistry};
use orca_harness_tools::ShellTool;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100);

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ShellTool::local()));
    let batch = |n: usize| {
        (0..n)
            .map(|i| call(&format!("c{i}"), "shell", json!({"command": "sleep 0.02"})))
            .collect::<Vec<_>>()
    };

    println!("\n{n} shell calls, each a real `sleep 0.02` subprocess\n");
    for (label, width) in [
        ("default (unbounded)", Limits::default().max_parallel_tools),
        ("old default (8)", 8),
    ] {
        let started = Instant::now();
        let results = Dispatcher::new()
            .execute(
                batch(n),
                &tools,
                &ExtensionRegistry::new(),
                &CancellationToken::new(),
                None,
                width,
            )
            .await
            .unwrap();
        let errors = results.iter().filter(|r| r.is_error).count();
        println!(
            "   {label:<22} {:>8.1} ms   errors {errors}",
            started.elapsed().as_secs_f64() * 1e3
        );
    }
    println!();
}
