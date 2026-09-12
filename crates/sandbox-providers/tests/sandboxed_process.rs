//! `process` driven through a real sandbox.
//!
//! The fake in `sandbox_workspace.rs` proves the command reaches the
//! provider. What it cannot prove is the part that took the work: a
//! process that stays alive across calls, takes stdin between them, and
//! reports its own exit. That needs a real container.
//!
//! Skips where no Docker daemon is available; it needs no API key.

use std::sync::Arc;

use orca_harness_core::{CancellationToken, Provisioner, Tool, ToolContext};
use orca_harness_sandbox_providers::{DockerProvisioner, EnvironmentSpec};
use orca_harness_tools::{Executor, ProcessTool};

async fn docker_available() -> bool {
    tokio::process::Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

fn ctx() -> ToolContext {
    ToolContext {
        call_id: "test".into(),
        tool_name: "process".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

#[tokio::test]
async fn background_processes_live_inside_the_sandbox() {
    if !docker_available().await {
        eprintln!("skipping: no Docker daemon available");
        return;
    }

    let sandbox = DockerProvisioner::new(
        EnvironmentSpec::new()
            .image("python:3.12-slim")
            .workspace_directory("/workspace"),
    )
    .start()
    .await
    .expect("start a docker sandbox");

    let process =
        Arc::new(ProcessTool::new(Executor::sandbox(sandbox.clone())).working_dir("/workspace"));

    // A command that runs to completion reports its own exit code, which
    // only works because the session surfaces a terminal event — the
    // output stream closing alone would give `None` here.
    let exited = process
        .call(
            serde_json::json!({
                "action": "spawn",
                "command": "echo hello && exit 3",
                "waitForExit": true
            }),
            &ctx(),
        )
        .await
        .expect("spawn");
    assert_eq!(exited["running"], false, "got {exited:?}");
    assert_eq!(exited["exitCode"], 3, "got {exited:?}");
    assert!(exited["output"].as_str().unwrap().contains("hello"));

    // A long-lived one stays alive between calls and takes stdin.
    let started = process
        .call(
            serde_json::json!({ "action": "spawn", "command": "python3 -i -u -q" }),
            &ctx(),
        )
        .await
        .expect("spawn python");
    assert_eq!(started["running"], true, "got {started:?}");
    let id = started["id"].as_str().expect("id").to_string();

    let written = process
        .call(
            serde_json::json!({
                "action": "write",
                "id": id,
                "input": "print(6 * 7)"
            }),
            &ctx(),
        )
        .await
        .expect("write");
    assert!(
        written["output"].as_str().unwrap().contains("42"),
        "stdin did not reach the process in the sandbox: {written:?}"
    );

    // It is the sandbox's filesystem it can see, not this machine's.
    process
        .call(
            serde_json::json!({
                "action": "write",
                "id": id,
                "input": "open('/workspace/from-process.txt','w').write('inside')"
            }),
            &ctx(),
        )
        .await
        .expect("write file");
    assert_eq!(
        sandbox
            .read_file("/workspace/from-process.txt")
            .await
            .expect("the process wrote inside the sandbox"),
        b"inside"
    );
    assert!(
        !std::path::Path::new("/workspace/from-process.txt").exists(),
        "the host filesystem must not have been touched"
    );

    // And it can be killed through the provider.
    let killed = process
        .call(serde_json::json!({ "action": "kill", "id": id }), &ctx())
        .await
        .expect("kill");
    assert_eq!(killed["running"], false, "got {killed:?}");

    drop(process);
    sandbox.shutdown().await.expect("shutdown");
}
