//! Persistent multi-turn session lifecycle: listing, resuming, forking,
//! clear-by-rotation, and reset-in-place.
//!
//! Demonstrates:
//! 1. Creating a persistent session and executing initial conversation turns.
//! 2. Listing persisted session files on disk via `Harness::sessions().list()`.
//! 3. Resuming an existing session by ID on a new agent instance to continue context.
//! 4. Forking a session into a distinct branch that inherits history without altering the parent.
//! 5. Clear-by-rotation (`session.clear()`) which rotates to a fresh session file.
//! 6. Destructive in-place reset (`session.reset_in_place()`) which clears history within the same session ID.

mod support;

use orca_harness_core::testing::ScriptedModel;
use orca_harness_core::ModelResponse;
use orca_harness_sdk::Harness;
use support::TempWorkspace;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempWorkspace::new("session-lifecycle");
    println!(
        "Initializing Harness in workspace: {}",
        workspace.path().display()
    );

    let harness = Harness::builder().workspace(workspace.path()).build()?;

    // -------------------------------------------------------------------------
    // Step 1: Create a persistent multi-turn session and run Turn 1
    // -------------------------------------------------------------------------
    println!("\n--- Step 1: Multi-turn Persistent Session ---");
    let model_t1 = ScriptedModel::new(vec![ModelResponse::final_text(
        "I remember your favorite language is Rust.",
    )]);

    let agent_t1 = harness
        .agent(model_t1)
        .name("assistant")
        .system_prompt("You are a helpful assistant.")
        .build()?;

    let session = agent_t1.new_session().persistent().open()?;
    let session_id = session.id().expect("persistent session must have an id");
    println!("Created persistent session ID: {session_id}");

    let res1 = session
        .run("Hello! My favorite programming language is Rust.")
        .await?;
    println!("Turn 1 response: {}", res1.text);
    assert_eq!(res1.text, "I remember your favorite language is Rust.");

    // -------------------------------------------------------------------------
    // Step 2: List sessions from the harness session manager
    // -------------------------------------------------------------------------
    println!("\n--- Step 2: List Sessions ---");
    let sessions_list = harness.sessions().list();
    println!("Found {} persisted session(s):", sessions_list.len());
    for s in &sessions_list {
        println!("  - Session ID: {}, Path: {}", s.meta.id, s.path.display());
    }
    assert!(
        sessions_list.iter().any(|s| s.meta.id == session_id),
        "Initial session must appear in sessions list"
    );

    // -------------------------------------------------------------------------
    // Step 3: Resume the session with a fresh agent and run Turn 2
    // -------------------------------------------------------------------------
    println!("\n--- Step 3: Resume Session ---");
    let model_t2 = ScriptedModel::new(vec![ModelResponse::final_text(
        "Cargo is Rust's official package manager and build tool.",
    )]);
    let agent_t2 = harness.agent(model_t2).name("assistant").build()?;

    let resumed_session = agent_t2.resume_session(&session_id)?;
    assert_eq!(resumed_session.id().as_deref(), Some(session_id.as_str()));

    let res2 = resumed_session
        .run("What build tool should I use with it?")
        .await?;
    println!("Turn 2 response: {}", res2.text);
    assert_eq!(
        res2.text,
        "Cargo is Rust's official package manager and build tool."
    );

    let messages = resumed_session.messages().await;
    // Expected messages: System, User (T1), Assistant (T1), User (T2), Assistant (T2)
    println!(
        "Total messages in resumed session context: {}",
        messages.len()
    );
    assert_eq!(
        messages.len(),
        5,
        "Context should preserve system and both conversational turns"
    );

    // -------------------------------------------------------------------------
    // Step 4: Fork the session into an independent branch
    // -------------------------------------------------------------------------
    println!("\n--- Step 4: Fork Session ---");
    let model_fork = ScriptedModel::new(vec![ModelResponse::final_text(
        "In this fork branch, Tokio is recommended for async runtimes.",
    )]);
    let agent_fork = harness.agent(model_fork).name("assistant").build()?;

    // Fork from the resumed session
    let forked_session = resumed_session.fork().await?;
    let forked_id = forked_session.id().expect("fork must have an id");
    println!("Forked new session ID: {forked_id}");
    assert_ne!(forked_id, session_id, "Fork must receive a new session ID");

    // Verify parentage in persisted metadata
    let forked_meta = harness
        .sessions()
        .list()
        .into_iter()
        .find(|s| s.meta.id == forked_id)
        .expect("forked session must be listed");
    assert_eq!(
        forked_meta.meta.parent.as_deref(),
        Some(session_id.as_str()),
        "Fork's parent must match original session ID"
    );

    // Resume forked session on agent_fork to run a turn on the fork branch
    let forked_active = agent_fork.resume_session(&forked_id)?;
    let res_fork = forked_active
        .run("What async runtime should I use?")
        .await?;
    println!("Fork turn response: {}", res_fork.text);
    assert_eq!(
        res_fork.text,
        "In this fork branch, Tokio is recommended for async runtimes."
    );

    // Fork should have 7 messages: System, 2 turns from parent, 1 turn in fork
    assert_eq!(forked_active.messages().await.len(), 7);
    // Parent should still have 5 messages (isolated from fork)
    assert_eq!(resumed_session.messages().await.len(), 5);

    // -------------------------------------------------------------------------
    // Step 5: Clear-by-rotation (session.clear())
    // -------------------------------------------------------------------------
    println!("\n--- Step 5: Clear-by-rotation (session.clear()) ---");
    let model_after_clear = ScriptedModel::new(vec![ModelResponse::final_text(
        "Fresh conversation after clear.",
    )]);
    let agent_clear = harness
        .agent(model_after_clear)
        .name("assistant")
        .system_prompt("You are a helpful assistant.")
        .build()?;

    let session_to_clear = agent_clear.resume_session(&session_id)?;
    let old_id = session_to_clear.id().unwrap();
    session_to_clear.clear().await?;
    let rotated_id = session_to_clear.id().unwrap();

    println!("Original ID before clear: {old_id}");
    println!("New ID after clear rotation: {rotated_id}");
    assert_ne!(
        old_id, rotated_id,
        "clear() rotates to a new session file with a fresh ID"
    );

    // Messages should be reset to only initial system prompt
    let cleared_msgs = session_to_clear.messages().await;
    assert_eq!(
        cleared_msgs.len(),
        1,
        "Cleared session should retain only system prompt"
    );

    let res_cleared = session_to_clear.run("Hello afresh!").await?;
    println!("Response in rotated session: {}", res_cleared.text);
    assert_eq!(res_cleared.text, "Fresh conversation after clear.");

    // -------------------------------------------------------------------------
    // Step 6: In-place Reset (session.reset_in_place())
    // -------------------------------------------------------------------------
    println!("\n--- Step 6: In-place Reset (session.reset_in_place()) ---");
    let model_after_reset = ScriptedModel::new(vec![ModelResponse::final_text(
        "Fresh response inside same session ID.",
    )]);
    let agent_reset = harness
        .agent(model_after_reset)
        .name("assistant")
        .system_prompt("You are a helpful assistant.")
        .build()?;

    let session_to_reset = agent_reset.resume_session(&rotated_id)?;
    let pre_reset_id = session_to_reset.id().unwrap();

    session_to_reset.reset_in_place().await?;
    let post_reset_id = session_to_reset.id().unwrap();

    println!("ID before reset_in_place: {pre_reset_id}");
    println!("ID after reset_in_place:  {post_reset_id}");
    assert_eq!(
        pre_reset_id, post_reset_id,
        "reset_in_place() retains the exact same session ID"
    );

    let reset_msgs = session_to_reset.messages().await;
    assert_eq!(
        reset_msgs.len(),
        1,
        "Reset session should retain only system prompt"
    );

    let res_reset = session_to_reset.run("Hello after reset!").await?;
    println!("Response after reset_in_place: {}", res_reset.text);
    assert_eq!(res_reset.text, "Fresh response inside same session ID.");

    println!("\nAll session lifecycle assertions passed successfully!");
    Ok(())
}
