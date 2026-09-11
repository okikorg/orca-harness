//! Typed background-process operations through a session: the host
//! handle and the `process` model tool share one session-owned manager,
//! and that manager dies with the session.

use std::time::Duration;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::ModelResponse;
use orca_harness_sdk::orchestration::{ProcessSpawn, ProcessWrite};
use orca_harness_sdk::{Harness, SdkError, ToolPreset};
use serde_json::json;

mod common;
use common::temp_dir;

#[tokio::test]
async fn coding_preset_exposes_processes_and_shellless_does_not() {
    let root = temp_dir("processes-presets");
    let harness = Harness::builder().workspace(&root).build().unwrap();

    let coding = harness
        .agent(ScriptedModel::new(Vec::new()))
        .tools(ToolPreset::Coding)
        .build()
        .unwrap();
    let session = coding.new_session().ephemeral().open().unwrap();
    let processes = session
        .processes()
        .expect("coding preset has a process tool");
    assert!(processes.is_open());
    assert!(processes.list().unwrap().is_empty());

    for preset in [
        ToolPreset::ShellLess,
        ToolPreset::ReadOnly,
        ToolPreset::None,
    ] {
        let agent = harness
            .agent(ScriptedModel::new(Vec::new()))
            .tools(preset)
            .build()
            .unwrap();
        let session = agent.new_session().ephemeral().open().unwrap();
        assert!(
            session.processes().is_none(),
            "{preset:?} must not expose processes"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn host_spawn_is_visible_to_the_model_tool_and_vice_versa() {
    let root = temp_dir("processes-shared");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call("l1", "process", json!({"action": "list"}))],
            usage: None,
        },
        ModelResponse::final_text("listed"),
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call(
                "s1",
                "process",
                json!({"action": "spawn", "command": "cat"}),
            )],
            usage: None,
        },
        ModelResponse::final_text("spawned"),
    ]);
    let agent = harness
        .agent(model)
        .tools(ToolPreset::Coding)
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let processes = session.processes().unwrap();

    let host = processes
        .spawn(ProcessSpawn::new("printf host-ready; sleep 5"), None)
        .await
        .unwrap();
    assert!(host.running);
    assert_eq!(host.output, "host-ready");

    let listed = session.run("list").await.unwrap();
    assert_eq!(listed.text, "listed");
    let transcript: Vec<String> = listed
        .messages
        .iter()
        .map(|message| serde_json::to_string(message).unwrap())
        .collect();
    assert!(
        transcript
            .iter()
            .any(|m| m.contains("processes") && m.contains(&host.id) && m.contains("host-ready")),
        "model `list` must see the host-spawned id {}: {transcript:?}",
        host.id
    );

    let spawned = session.run("spawn").await.unwrap();
    assert_eq!(spawned.text, "spawned");
    let entries = processes.list().unwrap();
    let model_entry = entries
        .iter()
        .find(|entry| entry.command == "cat")
        .expect("host list sees the model-spawned process");
    assert!(model_entry.running);
    assert_ne!(model_entry.id, host.id);

    // The host drives the model's process: same ids, same state.
    let echoed = processes
        .write(&model_entry.id, ProcessWrite::new("ping").eof(true))
        .await
        .unwrap();
    assert_eq!(echoed.output, "ping\n");
    assert!(!echoed.running);

    processes.kill(&host.id).await.unwrap();
    processes.kill(&model_entry.id).await.unwrap();
    assert!(processes.list().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn processes_are_per_session_and_die_with_the_session() {
    let root = temp_dir("processes-per-session");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(ScriptedModel::new(Vec::new()))
        .tools(ToolPreset::Coding)
        .build()
        .unwrap();
    let a = agent.new_session().ephemeral().open().unwrap();
    let b = agent.new_session().ephemeral().open().unwrap();
    let a_processes = a.processes().unwrap();
    let b_processes = b.processes().unwrap();

    let spawned = a_processes
        .spawn(ProcessSpawn::new("sleep 271.3 & wait"), None)
        .await
        .unwrap();
    assert_eq!(a_processes.list().unwrap().len(), 1);
    assert!(
        b_processes.list().unwrap().is_empty(),
        "session B must not see session A's process"
    );
    assert!(matches!(
        b_processes.poll(&spawned.id, None).await,
        Err(SdkError::Process(message)) if message.contains("unknown process id")
    ));

    drop(a);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!a_processes.is_open(), "the kept handle reports closed");
    assert!(matches!(
        a_processes.list(),
        Err(SdkError::Process(message)) if message == "process manager is closed"
    ));
    let found = std::process::Command::new("pgrep")
        .args(["-f", "sleep 271.3"])
        .output()
        .unwrap();
    assert!(
        !found.status.success(),
        "session A's process must die with session A"
    );
    assert!(b_processes.is_open());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn shutdown_closes_processes() {
    let root = temp_dir("processes-shutdown");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(ScriptedModel::new(Vec::new()))
        .tools(ToolPreset::Coding)
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let processes = session.processes().unwrap();

    session.shutdown(Duration::from_secs(1)).await.unwrap();
    assert!(matches!(
        processes.spawn(ProcessSpawn::new("true"), None).await,
        Err(SdkError::SessionClosed)
    ));
    assert!(matches!(processes.list(), Err(SdkError::SessionClosed)));
    assert!(!processes.is_open());

    let _ = std::fs::remove_dir_all(&root);
}
