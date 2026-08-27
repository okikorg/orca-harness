//! Complete, hermetic host assembly example.
//!
//! This example shows the pieces a host application normally wires together:
//! workspace and state, memory, skills, MCP, tools, policy, events, usage, and
//! a persistent session. `ScriptedModel` keeps it deterministic and offline.

mod support;

use std::sync::{Arc, Mutex};

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{FnTool, ModelResponse, ToolCall, Usage};
use orca_harness_extensions::{HarnessEvent, PolicyOutcome, ToolPolicy};
use orca_harness_sdk::{Harness, MemoryConfig, RunRequest, SkillDestination};
use serde_json::json;
use support::TempWorkspace;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Host-owned directories are explicit here. In a real host, use the
    // application's workspace and state directory instead of a temp folder.
    let workspace = TempWorkspace::new("host-assembly");
    let harness = Harness::builder()
        .workspace(workspace.path())
        .state_dir(workspace.path().join(".orca"))
        .build()?;
    println!("Workspace: {}", harness.workspace().root().display());
    println!("State: {}", harness.state_dir().display());

    // Durable host state: memory and discovered skills.
    let memory = harness.memory()?;
    memory.save(
        "The user prefers concise Rust answers.",
        "preference",
        false,
        "host_setup",
    )?;
    let skills = harness.skills();
    skills.scaffold("release_checklist", SkillDestination::Workspace)?;
    skills.reload();
    println!("Skills discovered: {}", skills.catalog().skills.len());

    // MCP is assembled even when no remote server is connected. This exposes
    // the MCP catalog interface and makes adding a server a host decision.
    let mcp = harness.mcp();
    println!("MCP interface tools: {}", mcp.tools().len());

    // A host-defined tool and a host-defined policy.
    let summarize = FnTool::new(
        "summarize_request",
        "Return a concise summary of a request",
        json!({
            "type": "object",
            "properties": {"request": {"type": "string"}},
            "required": ["request"]
        }),
        |args, _ctx| async move {
            let request = args["request"].as_str().unwrap_or("");
            Ok(json!({"summary": format!("Summary: {request}")}))
        },
    );
    let policy = ToolPolicy::new().rule(|call: &ToolCall| {
        if call.name == "dangerous_operation" {
            PolicyOutcome::Deny("dangerous_operation is disabled by the host".into())
        } else {
            PolicyOutcome::Allow
        }
    });

    // Replace this scripted model with OpenAiModel/OpenRouterModel/etc. in a
    // networked host. The rest of the assembly remains the same.
    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("I will summarize that request.".into()),
            calls: vec![call(
                "summary-1",
                "summarize_request",
                json!({"request": "prepare a concise Rust release checklist"}),
            )],
            usage: Some(Usage {
                input_tokens: 12,
                output_tokens: 7,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
            }),
        },
        ModelResponse::Final {
            text: "Ready: a concise Rust release checklist, using your saved preference.".into(),
            usage: Some(Usage {
                input_tokens: 24,
                output_tokens: 14,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
            }),
        },
    ]);

    let agent = harness
        .agent(model)
        .name("host-assistant")
        .system_prompt("You are the assistant embedded in a host application.")
        .tool(summarize)
        .policy(policy)
        .events(true)
        .usage(true)
        .memory(MemoryConfig::new(memory).automatic_recall(true))
        .skills(skills)
        .mcp(mcp)
        .build()?;

    // Persistent sessions let the host resume this conversation later.
    let session = agent.new_session().persistent().open()?;
    println!("Session: {:?}", session.id());

    let events = Arc::new(Mutex::new(Vec::new()));
    let event_log = Arc::clone(&events);
    let request =
        RunRequest::new("Prepare a concise Rust release checklist").on_event(move |event| {
            if let Ok(mut log) = event_log.lock() {
                log.push(event.clone());
            }
            if let HarnessEvent::ToolCall { tool_name, .. } = event {
                println!("event: tool call -> {tool_name}");
            }
        });

    let result = session.run(request).await?;
    println!("Response: {}", result.text);
    println!("Steps: {}", result.metered_steps);
    println!(
        "Usage: {} input, {} output",
        result.usage.input_tokens, result.usage.output_tokens
    );
    let event_count = events
        .lock()
        .map_err(|_| "event log mutex was poisoned")?
        .len();
    println!("Events received: {event_count}");
    assert!(result.text.contains("release checklist"));
    assert_eq!(result.metered_steps, 2);
    assert_eq!(result.usage.input_tokens, 36);
    assert_eq!(result.usage.output_tokens, 21);
    Ok(())
}
