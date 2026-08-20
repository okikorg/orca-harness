//! Basic benchmark suite. The key metric is not binary startup time: it is
//! the overhead added between a model emitting tool calls and those tools
//! doing useful work — dispatch latency, fan-out, and Extension cost.

use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use serde_json::{json, Value};

use orca_harness_core::testing::call;
use orca_harness_core::{
    CancellationToken, Concurrency, Dispatcher, Extension, ExtensionRegistry, FnTool,
    Subscriptions, Tool, ToolCall, ToolRegistry,
};

fn noop_tool() -> FnTool {
    FnTool::new(
        "noop",
        "does nothing",
        json!({"type": "object"}),
        |_input, _ctx| async move { Ok(Value::Null) },
    )
}

fn keyed_tool() -> FnTool {
    FnTool::new(
        "keyed",
        "keyed noop",
        json!({"type": "object"}),
        |_input, _ctx| async move { Ok(Value::Null) },
    )
    .concurrency(|input| Concurrency::Keyed(input["key"].as_str().unwrap_or("default").to_string()))
}

struct NoopExtension;

#[async_trait::async_trait]
impl Extension for NoopExtension {
    fn name(&self) -> &str {
        "noop"
    }
    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none()
            .before_tool()
            .around_tool()
            .after_tool()
            .tool_result()
    }
}

fn registry_with(tool: impl Tool + 'static) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(tool));
    tools
}

fn calls(n: usize, name: &str, args: impl Fn(usize) -> Value) -> Vec<ToolCall> {
    (0..n)
        .map(|i| call(&format!("call_{i}"), name, args(i)))
        .collect()
}

fn bench_dispatch(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dispatcher = Dispatcher::new();

    let mut group = c.benchmark_group("dispatch");
    for n in [1usize, 10, 100] {
        let tools = registry_with(noop_tool());
        let extensions = ExtensionRegistry::new();
        group.bench_with_input(BenchmarkId::new("noop_calls", n), &n, |b, &n| {
            b.to_async(&rt).iter(|| {
                let batch = calls(n, "noop", |_| json!({}));
                let tools = &tools;
                let extensions = &extensions;
                let dispatcher = &dispatcher;
                async move {
                    dispatcher
                        .execute(
                            batch,
                            tools,
                            extensions,
                            &CancellationToken::new(),
                            None,
                            16,
                        )
                        .await
                        .unwrap()
                }
            });
        });
    }
    group.finish();

    let mut group = c.benchmark_group("extensions");
    for n_ext in [0usize, 1, 10] {
        let tools = registry_with(noop_tool());
        let mut extensions = ExtensionRegistry::new();
        for _ in 0..n_ext {
            extensions.register(Arc::new(NoopExtension));
        }
        group.bench_with_input(
            BenchmarkId::new("ten_calls_with_exts", n_ext),
            &n_ext,
            |b, _| {
                b.to_async(&rt).iter(|| {
                    let batch = calls(10, "noop", |_| json!({}));
                    let tools = &tools;
                    let extensions = &extensions;
                    let dispatcher = &dispatcher;
                    async move {
                        dispatcher
                            .execute(
                                batch,
                                tools,
                                extensions,
                                &CancellationToken::new(),
                                None,
                                16,
                            )
                            .await
                            .unwrap()
                    }
                });
            },
        );
    }
    group.finish();

    let mut group = c.benchmark_group("keyed");
    let tools = registry_with(keyed_tool());
    let extensions = ExtensionRegistry::new();
    group.bench_function("100_calls_4_keys", |b| {
        b.to_async(&rt).iter(|| {
            let batch = calls(100, "keyed", |i| json!({"key": format!("k{}", i % 4)}));
            let tools = &tools;
            let extensions = &extensions;
            let dispatcher = &dispatcher;
            async move {
                dispatcher
                    .execute(
                        batch,
                        tools,
                        extensions,
                        &CancellationToken::new(),
                        None,
                        16,
                    )
                    .await
                    .unwrap()
            }
        });
    });
    group.finish();
}

criterion_group!(benches, bench_dispatch);
criterion_main!(benches);
