//! Skills + MCP interface tools, with memory deliberately omitted.
//!
//! This hermetic example scaffolds one workspace skill, exposes it alongside
//! the three lazy MCP catalog interfaces, and has a scripted model call the
//! `skill` and `mcp_search_tools` tools in one turn. No MCP server, network,
//! provider credential, or memory store is required.

mod support;

use std::sync::{Arc, Mutex};

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::ModelResponse;
use orca_harness_extensions::HarnessEvent;
use orca_harness_sdk::{Harness, SkillDestination, Skills};
use serde_json::json;
use support::TempWorkspace;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempWorkspace::new("skills-mcp-interface");
    let harness = Harness::builder().workspace(workspace.path()).build()?;

    // Create and discover one workspace-owned skill. Supplying no home root
    // keeps the example isolated from skills installed by the current user.
    let skills = Skills::new(
        harness.workspace().root(),
        Some(harness.state_dir().to_path_buf()),
        None,
    );
    let skill_file = skills.scaffold("release_notes", SkillDestination::Workspace)?;
    std::fs::write(
        &skill_file,
        "---\nname: release_notes\ndescription: Draft concise release notes\n---\n\nSummarize user-visible changes and verification evidence.\n",
    )?;
    let discovered = skills.reload();
    assert_eq!(discovered.skills.len(), 1);
    assert_eq!(discovered.skills[0].name, "release_notes");

    // With no connected servers, MCP still provides its three lazy catalog
    // interfaces. Searching the empty catalog is a valid, deterministic call.
    let mcp = harness.mcp();
    let interface_names: Vec<String> = mcp.tools().iter().map(|tool| tool.schema().name).collect();
    assert_eq!(
        interface_names,
        ["mcp_search_tools", "mcp_select_tool", "mcp_features"]
    );
    assert!(mcp.servers().is_empty());

    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("I will load the skill and inspect the MCP catalog.".into()),
            calls: vec![
                call("skill-1", "skill", json!({"name": "release_notes"})),
                call(
                    "mcp-1",
                    "mcp_search_tools",
                    json!({"query": "release notes"}),
                ),
            ],
            usage: None,
        },
        ModelResponse::final_text(
            "Loaded the release-notes skill; no matching MCP server tools are configured.",
        ),
    ]);

    // Deliberately no `.memory(...)`: this agent has only the skill tool and
    // MCP interfaces configured by the host.
    let agent = harness
        .agent(model)
        .name("skills-mcp-agent")
        .skills(skills)
        .mcp(mcp)
        .build()?;

    let outputs = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&outputs);
    let result = agent
        .new_session()
        .ephemeral()
        .open()?
        .run(
            orca_harness_sdk::RunRequest::new("Prepare release notes using available interfaces")
                .on_event(move |event| {
                    if let HarnessEvent::ToolResult {
                        tool_name, output, ..
                    } = event
                    {
                        captured
                            .lock()
                            .expect("event log lock")
                            .push((tool_name, output));
                    }
                }),
        )
        .await?;

    let outputs = outputs.lock().expect("event log lock");
    let skill_output = outputs
        .iter()
        .find(|(name, _)| name == "skill")
        .map(|(_, output)| output)
        .expect("skill result");
    assert_eq!(skill_output["name"], "release_notes");
    assert!(skill_output["instructions"]
        .as_str()
        .is_some_and(|text| text.contains("verification evidence")));

    let mcp_output = outputs
        .iter()
        .find(|(name, _)| name == "mcp_search_tools")
        .map(|(_, output)| output)
        .expect("MCP search result");
    assert_eq!(mcp_output, &json!({"tools": []}));

    // The Harness creates its state directory for skills and sessions, but no
    // memory database exists unless `harness.memory()` is opened explicitly.
    assert!(!harness.state_dir().join("memory.sqlite3").exists());

    println!("Skill loaded: release_notes");
    println!("MCP interfaces: {}", interface_names.join(", "));
    println!("MCP search matches: 0");
    println!("Memory database created: false");
    println!("Response: {}", result.text);
    Ok(())
}
