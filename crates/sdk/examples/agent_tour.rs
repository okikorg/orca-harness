//! Broad example showing Harness, AgentBuilder, ToolPreset, events, memory,
//! skills, and MCP configuration without requiring external services at runtime.

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{FnTool, ToolCall, Usage};
use orca_harness_extensions::{HarnessEvent, PolicyOutcome, ToolPolicy};
use orca_harness_sdk::{Harness, MemoryConfig, RunRequest, SkillDestination, Skills, ToolPreset};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Create a workspace Harness in a temporary folder
    let temp_dir = std::env::temp_dir().join(format!(
        "orca-sdk-example-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
    ));
    std::fs::create_dir_all(&temp_dir)?;

    let harness = Harness::builder().workspace(&temp_dir).build()?;
    println!(
        "Harness initialized at workspace: {}",
        harness.workspace().root().display()
    );

    // 2. Configure durable Memory
    let memory = harness.memory()?;
    memory.save(
        "User prefers concise answers with Rust examples.",
        "preference",
        false,
        "initial_setup",
    )?;
    println!("Saved initial user preference to SQLite memory.");

    // 3. Configure Skills
    let skills = Skills::new(
        harness.workspace().root(),
        Some(harness.state_dir().to_path_buf()),
        None,
    );
    skills.scaffold("rust_helper", SkillDestination::Workspace)?;
    skills.reload();
    println!("Scaffolded and discovered 'rust_helper' skill.");

    // 4. Configure local MCP
    let mcp = harness.mcp();
    println!(
        "Configured MCP manager (discovered {} interface tools)",
        mcp.tools().len()
    );

    // 5. Custom tool and security policy
    let custom_calc = FnTool::new(
        "calc",
        "calculate simple expressions",
        json!({
            "type": "object",
            "properties": {
                "expression": {"type": "string"}
            },
            "required": ["expression"]
        }),
        |args, _ctx| async move {
            let expr = args["expression"].as_str().unwrap_or("0");
            Ok(json!({ "result": format!("evaluated: {expr}") }))
        },
    );

    let policy = ToolPolicy::new().rule(|call: &ToolCall| {
        if call.name == "forbidden_tool" {
            PolicyOutcome::Deny("access to forbidden_tool denied".into())
        } else {
            PolicyOutcome::Allow
        }
    });

    // 6. Deterministic ScriptedModel with tool round and final response
    let model = ScriptedModel::new(vec![
        orca_harness_core::ModelResponse::ToolCalls {
            content: Some("Let me calculate that for you.".into()),
            calls: vec![call("c1", "calc", json!({"expression": "21 * 2"}))],
            usage: Some(Usage {
                input_tokens: 15,
                output_tokens: 8,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
            }),
        },
        orca_harness_core::ModelResponse::Final {
            text: "Calculation complete: 21 * 2 = 42. Recalled preference: concise Rust answers."
                .into(),
            usage: Some(Usage {
                input_tokens: 25,
                output_tokens: 18,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
            }),
        },
    ]);

    // 7. Compose Agent via AgentBuilder
    let agent = harness
        .agent(model)
        .name("example-agent")
        .system_prompt("You are a helpful coding assistant.")
        .tools(ToolPreset::Coding)
        .tool(custom_calc)
        .policy(policy)
        .events(true)
        .usage(true)
        .memory(MemoryConfig::new(memory).automatic_recall(true))
        .skills(skills)
        .mcp(mcp)
        .build()?;

    // 8. Open a persistent session and run with event streaming
    let session = agent.new_session().persistent().open()?;
    println!("Opened session: {:?}", session.id());

    let req = RunRequest::new("Calculate 21 * 2").on_event(|event| match event {
        HarnessEvent::ToolCall {
            tool_name, input, ..
        } => {
            println!("  [Event] Tool called: {tool_name} with {input}");
        }
        HarnessEvent::ToolResult {
            tool_name, output, ..
        } => {
            println!("  [Event] Tool result: {tool_name} -> {output}");
        }
        _ => {}
    });

    let result = session.run(req).await?;
    println!("\nAgent Result:\n{}", result.text);
    println!(
        "Metered steps: {}, Input tokens: {}, Output tokens: {}",
        result.metered_steps, result.usage.input_tokens, result.usage.output_tokens
    );

    // Clean up
    let _ = std::fs::remove_dir_all(&temp_dir);
    Ok(())
}
