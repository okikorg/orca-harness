//! Demonstrates memory CRUD, automatic recall, and the full skill lifecycle
//! (scaffold -> discover -> enable -> disable -> uninstall) inside one hermetic
//! run that never touches the network.

use std::sync::Arc;

use orca_harness_core::testing::ScriptedModel;
use orca_harness_core::{FnTool, ModelResponse, Usage};
use orca_harness_sdk::{Harness, MemoryConfig, SkillDestination, Skills, ToolPreset};
use serde_json::json;

fn temp_dir(_label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "orca-mem-skills-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// 1 · MEMORY CRUD
// ---------------------------------------------------------------------------

fn memory_crud(harness: &Harness) {
    println!("=== Memory CRUD ===");
    let mem = harness.memory().unwrap();

    // Create
    let r1 = mem
        .save("Favorite language: Rust", "preference", false, "setup")
        .unwrap();
    println!(
        "  Created: id={} content=\"{}\" kind={}",
        r1.id, r1.content, r1.kind
    );

    let r2 = mem
        .save(
            "Deployment target: AWS us-east-1",
            "workflow",
            false,
            "setup",
        )
        .unwrap();
    println!("  Created: id={} content=\"{}\"", r2.id, r2.content);

    // List
    let all = mem.list(10).unwrap();
    println!("  Listed {} records", all.len());
    assert_eq!(all.len(), 2);

    // Search
    let hits = mem.search("Rust", 5).unwrap();
    println!("  Search 'Rust' matched {} record(s)", hits.len());
    assert_eq!(hits[0].id, r1.id);

    // Update
    let updated = mem
        .update(
            &r2.id,
            "Deployment target: AWS eu-central-1",
            Some("workflow"),
        )
        .unwrap()
        .unwrap();
    println!(
        "  Updated: content=\"{}\" kind={}",
        updated.content, updated.kind
    );
    assert_eq!(updated.content, "Deployment target: AWS eu-central-1");

    // Forget
    let gone = mem.forget(&r2.id).unwrap();
    println!("  Forgotten: {}", gone);
    assert!(gone);

    let remaining = mem.list(10).unwrap();
    println!("  After forget: {} record(s)", remaining.len());
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, r1.id);

    println!("  OK - Memory CRUD complete\n");
}

// ---------------------------------------------------------------------------
// 2 · SKILLS LIFECYCLE
// ---------------------------------------------------------------------------

fn skills_lifecycle(harness: &Harness) {
    println!("=== Skills Lifecycle ===");
    // Pass home=None so real user skill directories don't leak in.
    let skills = Skills::new(
        harness.workspace().root(),
        Some(harness.state_dir().to_path_buf()),
        None,
    );

    // Scaffold into workspace directory
    let path = skills
        .scaffold("weather_skill", SkillDestination::Workspace)
        .unwrap();
    println!("  Scaffolded 'weather_skill' at {}", path.display());
    assert!(path.exists());

    // Discover via reload
    let discovered = skills.reload();
    println!("  Discovered {} skill(s):", discovered.skills.len());
    for s in &discovered.skills {
        println!("    - {} ({})", s.name, s.dir.display());
    }
    assert_eq!(discovered.skills.len(), 1);
    assert!(discovered.skills.iter().any(|s| s.name == "weather_skill"));

    // Catalog mirrors discovery
    let catalog = skills.catalog();
    assert_eq!(catalog.skills.len(), 1);

    // Tool is available while enabled
    if let Some(ref tool) = skills.tool() {
        println!("  Tool exposed: {:?}", tool.schema().name);
    } else {
        panic!("tool should exist when skill is enabled");
    }
    assert!(skills.tool().is_some());

    // Disable
    skills.disable("weather_skill");
    assert!(skills.tool().is_none());
    println!("  Disabled -> tool() = None");

    // Re-enable
    skills.enable("weather_skill");
    assert!(skills.tool().is_some());
    println!("  Enabled again -> tool() present");

    // Uninstall
    let uninstalled = skills.uninstall("weather_skill").unwrap();
    assert!(uninstalled);
    println!("  Uninstalled");

    // Reload reflects removal
    let reloaded = skills.reload();
    assert!(reloaded.skills.is_empty());
    assert!(skills.tool().is_none());
    println!(
        "  After reload: {} skill(s), tool={}",
        reloaded.skills.len(),
        skills.tool().is_some()
    );

    println!("  OK - Skills lifecycle complete\n");
}

// ---------------------------------------------------------------------------
// 3 · AGENT WITH AUTO RECALL
// ---------------------------------------------------------------------------

async fn auto_recall_agent(harness: &Harness) {
    println!("=== Agent with Automatic Memory Recall ===");

    // Pre-populate some preferences
    let mem = harness.memory().unwrap();
    mem.save(
        "User prefers short answers.",
        "preference",
        false,
        "initial",
    )
    .unwrap();
    mem.save("Always mention Rust first.", "fact", false, "initial")
        .unwrap();

    // Custom tool: the scripted model calls this to "acknowledge" memories
    let ack_mem = FnTool::new(
        "acknowledge_memory",
        "Acknowledges stored memories",
        json!({"type": "object", "properties": {"msg": {"type": "string"}}}),
        |args, _ctx| async move {
            let msg = args["msg"].as_str().unwrap_or("");
            Ok(json!({"acknowledged": msg}))
        },
    );

    // Scripted model: calls ack once, then returns final answer referencing memory
    let model = Arc::new(ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("Recalling preferences...".into()),
            calls: vec![orca_harness_core::testing::call(
                "m1",
                "acknowledge_memory",
                json!({"msg": "I remember you prefer short answers."}),
            )],
            usage: Some(Usage {
                input_tokens: 20,
                output_tokens: 6,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
            }),
        },
        ModelResponse::Final {
            text: "Short. Rust first. Done.".into(),
            usage: Some(Usage {
                input_tokens: 30,
                output_tokens: 12,
                cache_read_tokens: 0,
                cache_create_tokens: 0,
            }),
        },
    ]));

    let agent = harness
        .agent(model.clone())
        .name("recall-agent")
        .system_prompt("You are helpful and concise.")
        .tools(ToolPreset::ReadOnly)
        .tool(ack_mem)
        .events(true)
        .memory(MemoryConfig::read_write(mem)) // search + manage + auto-recall
        .build()
        .unwrap();

    let session = agent.new_session().ephemeral().open().unwrap();
    let result = session
        .run("Which language should you mention first?")
        .await
        .unwrap();

    println!("  Final text: \"{}\"", result.text);
    assert!(result.text.to_lowercase().contains("rust"));
    let observed = model.observed_contexts();
    let injected = observed.iter().any(|context| {
        serde_json::to_string(context.messages())
            .unwrap_or_default()
            .contains("Always mention Rust first")
    });
    assert!(injected, "automatic recall must reach the model context");
    println!("  OK - Auto-recall agent complete\n");
}

// ---------------------------------------------------------------------------
// MAIN
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = temp_dir("memory-skills");

    let harness = Harness::builder().workspace(&root).build()?;
    println!(
        "Memory + Skills Example --- workspace: {}\n",
        harness.workspace().root().display()
    );

    memory_crud(&harness);
    skills_lifecycle(&harness);
    auto_recall_agent(&harness).await;

    std::fs::remove_dir_all(&root)?;
    println!("Temp dir cleaned up.");
    Ok(())
}
