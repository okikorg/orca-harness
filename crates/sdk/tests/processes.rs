//! Typed background-process operations through a session: the host
//! handle and the `process` model tool share one session-owned manager,
//! process events reach the host on the session's notification channel,
//! clear kills while shutdown kills and closes, and that manager dies
//! with the session.

use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{CancellationToken, Extension, ModelResponse};
use orca_harness_sdk::orchestration::{ProcessSpawn, ProcessWrite, SubagentRequest};
use orca_harness_sdk::{
    BackgroundNotification, Executor, Harness, ProcessConfig, ProcessNotification,
    ProcessNotificationKind, SdkError, SubagentConfig, ToolPreset,
};
use serde_json::json;
use tokio::sync::Notify;

mod background_support;
mod common;
use background_support::{Held, Stall};
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

/// The next process notification, skipping the others, within a bound.
async fn next_process_notification(
    receiver: &mut tokio::sync::broadcast::Receiver<BackgroundNotification>,
    what: &str,
) -> ProcessNotification {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let notification = tokio::time::timeout_at(deadline, receiver.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .unwrap_or_else(|error| panic!("channel closed waiting for {what}: {error}"));
        if let BackgroundNotification::ProcessNotified(notification) = notification {
            return notification;
        }
    }
}

#[tokio::test]
async fn readiness_and_exit_notifications_reach_the_host() {
    let root = temp_dir("processes-notify");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(ScriptedModel::new(Vec::new()))
        .tools(ToolPreset::Coding)
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let mut notifications = session.notifications();
    let processes = session.processes().unwrap();

    let spawned = processes
        .spawn(
            ProcessSpawn::new("sleep 1; echo ready; sleep 0.3; echo bye").notify_on_match("ready"),
            None,
        )
        .await
        .unwrap();
    assert!(spawned.running);

    let ready = next_process_notification(&mut notifications, "the readiness match").await;
    assert_eq!(ready.id, spawned.id);
    assert_eq!(
        ready.kind,
        ProcessNotificationKind::OutputMatch {
            pattern: "ready".into()
        }
    );
    assert!(ready.output.contains("ready"), "{ready:?}");

    let exit = next_process_notification(&mut notifications, "the exit").await;
    assert_eq!(exit.id, spawned.id);
    assert_eq!(exit.command, "sleep 1; echo ready; sleep 0.3; echo bye");
    assert_eq!(
        exit.kind,
        ProcessNotificationKind::Exit { exit_code: Some(0) }
    );
    assert!(exit.output.contains("bye"), "{exit:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn clear_kills_background_processes_but_keeps_the_handle_open() {
    let root = temp_dir("processes-clear");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(ScriptedModel::new(Vec::new()))
        .tools(ToolPreset::Coding)
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let mut notifications = session.notifications();
    let processes = session.processes().unwrap();

    processes
        .spawn(ProcessSpawn::new("sleep 285.3 & wait"), None)
        .await
        .unwrap();
    assert_eq!(processes.list().unwrap().len(), 1);

    session.clear().await.unwrap();
    assert!(processes.list().unwrap().is_empty());
    assert!(processes.is_open());
    let found = std::process::Command::new("pgrep")
        .args(["-f", "sleep 285.3"])
        .output()
        .unwrap();
    assert!(!found.status.success(), "clear must kill the process");
    assert!(
        matches!(
            notifications.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "a cleared process reports no exit"
    );

    let again = processes
        .spawn(ProcessSpawn::new("printf after-clear"), None)
        .await
        .unwrap();
    assert_eq!(again.output, "after-clear");
    assert_eq!(processes.list().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn shutdown_kills_processes_and_reports_stragglers() {
    let root = temp_dir("processes-shutdown-kill");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(ScriptedModel::new(Vec::new()))
        .tools(ToolPreset::Coding)
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let processes = session.processes().unwrap();
    processes
        .spawn(ProcessSpawn::new("sleep 286.4 & wait"), None)
        .await
        .unwrap();

    session.shutdown(Duration::from_secs(2)).await.unwrap();
    assert!(!processes.is_open());
    let found = std::process::Command::new("pgrep")
        .args(["-f", "sleep 286.4"])
        .output()
        .unwrap();
    assert!(!found.status.success(), "shutdown must kill the process");
    assert!(matches!(
        processes.spawn(ProcessSpawn::new("true"), None).await,
        Err(SdkError::SessionClosed)
    ));

    // A process cannot be made to survive SIGKILL, so the timeout path is
    // driven by a stalled subagent; the error still accounts for
    // processes, all of which were killed in time.
    let entered = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let agent = harness
        .agent(Held(release.clone()))
        .tools(ToolPreset::Coding)
        .subagents(SubagentConfig::new().child_extensions({
            let entered = entered.clone();
            Arc::new(move |_spawn| vec![Arc::new(Stall(entered.clone())) as Arc<dyn Extension>])
        }))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let processes = session.processes().unwrap();
    processes
        .spawn(ProcessSpawn::new("sleep 287.5 & wait"), None)
        .await
        .unwrap();
    session
        .subagents()
        .unwrap()
        .spawn(SubagentRequest::new("stalled"))
        .unwrap();
    entered.notified().await;
    let error = session.shutdown(Duration::from_secs(1)).await.unwrap_err();
    assert!(
        matches!(
            error,
            SdkError::ShutdownTimeout {
                still_active_workers: 1,
                still_running_processes: 0
            }
        ),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        "shutdown timed out with 1 background worker(s) and 0 process(es) still active"
    );
    assert!(!processes.is_open());
    let found = std::process::Command::new("pgrep")
        .args(["-f", "sleep 287.5"])
        .output()
        .unwrap();
    assert!(!found.status.success());
    release.cancel();

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn process_config_is_rejected_for_presets_without_process_execution() {
    let root = temp_dir("processes-config-rejected");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    for preset in [
        ToolPreset::ShellLess,
        ToolPreset::ReadOnly,
        ToolPreset::None,
    ] {
        let error = harness
            .agent(ScriptedModel::new(Vec::new()))
            .tools(preset)
            .processes(ProcessConfig::default())
            .build()
            .err()
            .unwrap_or_else(|| panic!("{preset:?} must reject process configuration"));
        assert!(
            matches!(&error, SdkError::Config(message) if message.contains("ToolPreset::Coding")),
            "{error}"
        );
    }
    let session = harness
        .agent(ScriptedModel::new(Vec::new()))
        .tools(ToolPreset::Coding)
        .processes(ProcessConfig::new().max_processes(1))
        .build()
        .unwrap()
        .new_session()
        .ephemeral()
        .open()
        .unwrap();
    let processes = session.processes().unwrap();
    processes
        .spawn(ProcessSpawn::new("sleep 5"), None)
        .await
        .unwrap();
    assert!(matches!(
        processes.spawn(ProcessSpawn::new("sleep 5"), None).await,
        Err(SdkError::Process(message)) if message.contains("live process limit reached (1)")
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn remote_executor_keeps_file_tools_local() {
    let root = temp_dir("processes-remote-executor");
    std::fs::write(root.join("note.md"), "local bytes").unwrap();
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call("r1", "read_file", json!({"path": "note.md"}))],
            usage: None,
        },
        ModelResponse::final_text("read"),
    ]);
    let agent = harness
        .agent(model)
        .tools(ToolPreset::Coding)
        .processes(ProcessConfig::new().executor(Executor::docker_exec("orca-sdk-never-run")))
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();
    let processes = session
        .processes()
        .expect("the remote process tool has a controller");
    assert!(processes.is_open());
    assert!(processes.list().unwrap().is_empty());

    let result = session.run("read the note").await.unwrap();
    assert_eq!(result.text, "read");
    let transcript: Vec<String> = result
        .messages
        .iter()
        .map(|message| serde_json::to_string(message).unwrap())
        .collect();
    assert!(
        transcript.iter().any(|m| m.contains("local bytes")),
        "read_file must read the local workspace: {transcript:?}"
    );
    assert!(!session.file_guard().is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn fork_and_resume_do_not_share_processes() {
    let root = temp_dir("processes-fork-resume");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let agent = harness
        .agent(ScriptedModel::new(vec![ModelResponse::final_text("hi")]))
        .tools(ToolPreset::Coding)
        .build()
        .unwrap();
    let session = agent.new_session().persistent().open().unwrap();
    session.run("hello").await.unwrap();
    let processes = session.processes().unwrap();
    let spawned = processes
        .spawn(ProcessSpawn::new("sleep 5"), None)
        .await
        .unwrap();

    let fork = session.fork().await.unwrap();
    let fork_processes = fork.processes().unwrap();
    assert!(fork_processes.list().unwrap().is_empty());
    assert!(matches!(
        fork_processes.poll(&spawned.id, None).await,
        Err(SdkError::Process(message)) if message.contains("unknown process id")
    ));

    let resumed = agent.resume_session(&session.id().unwrap()).unwrap();
    assert_eq!(resumed.messages().await.len(), 2);
    assert!(resumed.processes().unwrap().list().unwrap().is_empty());
    assert_eq!(
        processes.list().unwrap().len(),
        1,
        "the original still owns its process"
    );

    let _ = std::fs::remove_dir_all(&root);
}
