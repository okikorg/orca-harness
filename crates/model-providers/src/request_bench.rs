//! Offline timing harness: no request is sent and no credentials are loaded.
use orca_harness_core::{Context, Image, ToolCall, ToolResult, ToolSchema};
use serde_json::json;
use std::{hint::black_box, time::Instant};

pub(crate) fn fixtures() -> Vec<(&'static str, Context, Vec<ToolSchema>)> {
    let tools = vec![ToolSchema {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
    }];
    let mut small = Context::new();
    small.push_system("You are a coding assistant.");
    small.push_user("Read Cargo.toml and summarize the dependencies.");
    let mut history = small.clone();
    for i in 0..20 {
        let call = ToolCall {
            id: format!("call_{i}"),
            name: "read_file".into(),
            arguments: json!({"path":format!("src/file_{i}.rs")}),
        };
        history.push_assistant_tool_calls(None, vec![call.clone()]);
        history.append_tool_results(vec![ToolResult::ok(
            &call,
            json!({"text":"source line\n".repeat(700)}),
        )]);
    }
    let mut image = small.clone();
    image.push_user_with_images(
        "Inspect this screenshot",
        vec![Image {
            media_type: "image/png".into(),
            data: "AAAA".repeat(256 * 1024),
        }],
    );
    vec![
        ("small", small, tools.clone()),
        ("tool_history", history, tools.clone()),
        ("image_1mib", image, tools),
    ]
}

pub(crate) fn measure<T>(mut work: impl FnMut() -> T) -> f64 {
    for _ in 0..5 {
        black_box(work());
    }
    let mut samples = Vec::with_capacity(21);
    for _ in 0..21 {
        let start = Instant::now();
        for _ in 0..10 {
            black_box(work());
        }
        samples.push(start.elapsed().as_secs_f64() * 1e6 / 10.0);
    }
    samples.sort_by(f64::total_cmp);
    samples[10]
}
