//! Custom `FnTool` + live/ordered `HarnessEvent` callback + metered token usage.
//!
//! Demonstrates:
//! 1. Registering a custom asynchronous `FnTool` with input validation and execution.
//! 2. Attaching a live event callback via `RunRequest::on_event` to observe lifecycle events.
//! 3. Verifying the execution flow, event ordering, and final usage statistics.

mod support;

use std::sync::{Arc, Mutex};

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{FnTool, ModelResponse, Usage};
use orca_harness_extensions::HarnessEvent;
use orca_harness_sdk::{Harness, RunRequest};
use serde_json::json;
use support::TempWorkspace;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempWorkspace::new("custom-tool-events");
    println!(
        "Initializing Harness in workspace: {}",
        workspace.path().display()
    );

    let harness = Harness::builder().workspace(workspace.path()).build()?;

    // 1. Define a custom arithmetic tool
    let add_tool = FnTool::new(
        "calculate_sum",
        "Calculates the sum of two numbers 'a' and 'b'",
        json!({
            "type": "object",
            "properties": {
                "a": { "type": "number" },
                "b": { "type": "number" }
            },
            "required": ["a", "b"]
        }),
        |args, _ctx| async move {
            let a = args["a"].as_f64().unwrap_or(0.0);
            let b = args["b"].as_f64().unwrap_or(0.0);
            let sum = a + b;
            println!("  [Tool Execute] calculate_sum({a}, {b}) => {sum}");
            Ok(json!({ "sum": sum }))
        },
    );

    // 2. Configure scripted 2-step model response (tool call -> final response)
    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("Invoking calculate_sum for 12.5 and 29.5".into()),
            calls: vec![call(
                "call_1",
                "calculate_sum",
                json!({"a": 12.5, "b": 29.5}),
            )],
            usage: Some(Usage {
                input_tokens: 15,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
            }),
        },
        ModelResponse::Final {
            text: "The total sum is 42.0".into(),
            usage: Some(Usage {
                input_tokens: 30,
                output_tokens: 12,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
            }),
        },
    ]);

    // 3. Build the Agent with events and usage metering enabled
    let agent = harness
        .agent(model)
        .name("math-assistant")
        .system_prompt("You are a helpful math agent.")
        .tool(add_tool)
        .events(true)
        .usage(true)
        .build()?;

    let session = agent.new_session().ephemeral().open()?;

    // 4. Track events delivered in real-time to the callback
    let recorded_events = Arc::new(Mutex::new(Vec::new()));
    let events_collector = recorded_events.clone();

    let request = RunRequest::new("Please add 12.5 and 29.5").on_event(move |event| {
        events_collector.lock().unwrap().push(event.clone());
        match &event {
            HarnessEvent::AgentStart => println!("  [Event] AgentStart"),
            HarnessEvent::Assistant { message } => println!("  [Event] Assistant: {message}"),
            HarnessEvent::ToolCall {
                tool_name, input, ..
            } => {
                println!("  [Event] ToolCall: {tool_name}({input})")
            }
            HarnessEvent::ToolFinished {
                tool_name,
                is_error,
                ..
            } => {
                println!("  [Event] ToolFinished: {tool_name} (is_error: {is_error})")
            }
            HarnessEvent::ToolResult {
                tool_name, output, ..
            } => {
                println!("  [Event] ToolResult: {tool_name} -> {output}")
            }
            HarnessEvent::Usage { usage } => {
                println!(
                    "  [Event] Usage: {} in, {} out",
                    usage.input_tokens, usage.output_tokens
                )
            }
            HarnessEvent::Result { message } => println!("  [Event] Result: {message}"),
            _ => {}
        }
    });

    println!("\n--- Running prompt ---");
    let result = session.run(request).await?;

    println!("\n--- Verification ---");
    println!("Final response: {}", result.text);
    println!("Metered steps: {}", result.metered_steps);
    println!(
        "Total token usage: {} input, {} output",
        result.usage.input_tokens, result.usage.output_tokens
    );

    // Assert correctness
    assert_eq!(result.text, "The total sum is 42.0");
    assert_eq!(result.metered_steps, 2);
    assert_eq!(result.usage.input_tokens, 45); // 15 + 30
    assert_eq!(result.usage.output_tokens, 22); // 10 + 12

    let events = recorded_events.lock().unwrap().clone();
    assert!(!events.is_empty(), "Events should be captured");

    assert!(matches!(events.first(), Some(HarnessEvent::AgentStart)));

    let tool_call = events
        .iter()
        .position(|event| matches!(event, HarnessEvent::ToolCall { tool_name, .. } if tool_name == "calculate_sum"))
        .expect("calculate_sum ToolCall event");
    let tool_result = events
        .iter()
        .position(|event| matches!(event, HarnessEvent::ToolResult { tool_name, .. } if tool_name == "calculate_sum"))
        .expect("calculate_sum ToolResult event");
    let result_event = events
        .iter()
        .position(|event| matches!(event, HarnessEvent::Result { message } if message == "The total sum is 42.0"))
        .expect("final Result event");
    assert!(tool_call < tool_result && tool_result < result_event);

    println!("All assertions passed successfully!");
    Ok(())
}
