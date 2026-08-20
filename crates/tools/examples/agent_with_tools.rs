//! A self-contained agent that uses the core host tools, driven by a fake
//! LLM (no API key needed). Shows the intended assembly: kernel + tools +
//! critical extensions.
//!
//! Run with: cargo run -p orca-harness-tools --example agent_with_tools

use serde_json::json;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{Agent, ModelResponse};
use orca_harness_extensions::{EventStream, ToolPolicy, Truncation, UsageMeter};
use orca_harness_tools::{core_tools, Workspace};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join("orca-harness-example");
    std::fs::create_dir_all(&dir)?;
    let ws = Workspace::new(dir.clone());

    // A fake model that: writes a script, runs it via shell, then answers.
    let model = ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call(
            "c0",
            "write_file",
            json!({"path": "hello.sh", "content": "echo hello from the harness"}),
        )]),
        ModelResponse::tool_calls(vec![call("c1", "shell", json!({"command": "sh hello.sh"}))]),
        ModelResponse::final_text("Wrote and ran the script."),
    ]);

    let (events, mut rx) = EventStream::channel();
    let (meter, usage) = UsageMeter::new();

    let mut agent = Agent::new(model)
        .system_prompt("You are a coding agent with host tools.")
        .extension(events)
        .extension(ToolPolicy::new().deny(["rm"])) // example guardrail
        .extension(Truncation::default())
        .extension(meter);
    for tool in core_tools(&ws) {
        agent = agent.tool_arc(tool);
    }

    let answer = agent.run("Create hello.sh and run it").await?;

    // Drain the typed event stream (a sidecar would serialize these to NDJSON).
    while let Ok(event) = rx.try_recv() {
        println!("event: {}", serde_json::to_string(&event)?);
    }
    println!("\nanswer: {answer}");
    println!("usage: {:?}", usage.total());
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
