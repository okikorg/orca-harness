//! Background processes through a session: start one from the host,
//! hear about its readiness and its exit on the session's notification
//! channel, then shut the session down.
//!
//! Demonstrates:
//! 1. A `ToolPreset::Coding` agent with an explicit `ProcessConfig`
//!    (local executor, a live-process cap).
//! 2. `Session::processes()` spawning a detached command with a
//!    readiness pattern (`notify_on_match`).
//! 3. `Session::notifications()` delivering
//!    `BackgroundNotification::ProcessNotified` for the match and the
//!    exit, outside any model run.
//! 4. `Session::shutdown` killing what is left and closing the handle.
//!
//! Deterministic: the model is scripted and never called.

mod support;

use std::time::Duration;

use orca_harness_core::testing::ScriptedModel;
use orca_harness_sdk::orchestration::ProcessSpawn;
use orca_harness_sdk::{
    BackgroundNotification, Harness, ProcessConfig, ProcessNotification, ProcessNotificationKind,
    ToolPreset,
};
use support::TempWorkspace;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempWorkspace::new("background-processes");
    let harness = Harness::builder().workspace(workspace.path()).build()?;
    let agent = harness
        .agent(ScriptedModel::new(Vec::new()))
        .tools(ToolPreset::Coding)
        .processes(ProcessConfig::new().max_processes(4))
        .build()?;
    let session = agent.new_session().ephemeral().open()?;

    // Subscribe before spawning so no notification can slip past.
    let mut notifications = session.notifications();
    let processes = session
        .processes()
        .expect("the Coding preset ships the process tool");

    let spawned = processes
        .spawn(
            ProcessSpawn::new("sleep 1; echo ready; sleep 1; echo done").notify_on_match("ready"),
            None,
        )
        .await?;
    println!("spawned {} (running: {})", spawned.id, spawned.running);

    let ready = next_process_notification(&mut notifications).await?;
    print_notification("readiness", &ready);
    assert!(matches!(
        ready.kind,
        ProcessNotificationKind::OutputMatch { ref pattern } if pattern == "ready"
    ));

    let exit = next_process_notification(&mut notifications).await?;
    print_notification("exit", &exit);
    assert_eq!(
        exit.kind,
        ProcessNotificationKind::Exit { exit_code: Some(0) }
    );

    session.shutdown(Duration::from_secs(2)).await?;
    assert!(!processes.is_open());
    println!("session shut down; process handle closed");
    Ok(())
}

async fn next_process_notification(
    receiver: &mut tokio::sync::broadcast::Receiver<BackgroundNotification>,
) -> Result<ProcessNotification, Box<dyn std::error::Error>> {
    loop {
        let notification = tokio::time::timeout(Duration::from_secs(10), receiver.recv()).await??;
        if let BackgroundNotification::ProcessNotified(notification) = notification {
            return Ok(notification);
        }
    }
}

fn print_notification(label: &str, notification: &ProcessNotification) {
    println!(
        "{label}: {} {:?} output={:?}",
        notification.id,
        notification.kind,
        notification.output.trim_end()
    );
}
