//! Explicitly invoked local Docker evidence; no model/provider calls.
use orca_harness_core::{ExecRequest, Provisioner, Sandbox, SpawnRequest};
use orca_harness_sandbox_providers::{DockerProvisioner, EnvironmentSpec, Network};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(std::io::Error::other(message).into())
    }
}
async fn scenarios(sandbox: Arc<dyn Sandbox>) -> Result<()> {
    let (interactive, mut interactive_output) = sandbox
        .spawn(SpawnRequest {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "printf ready >&2; read value; printf '%s' \"$value\"".into(),
            ],
            ..Default::default()
        })
        .await?;
    let ready = tokio::time::timeout(Duration::from_secs(2), interactive_output.recv())
        .await?
        .ok_or_else(|| std::io::Error::other("interactive stderr ended"))?;
    require(
        ready.stderr && ready.bytes == b"ready",
        "interactive stderr was withheld",
    )?;
    interactive.write_stdin(b"go\n").await?;
    interactive.close_stdin().await?;
    let mut echoed = Vec::new();
    while let Some(chunk) = interactive_output.recv().await {
        if !chunk.stderr {
            echoed.extend(chunk.bytes);
        }
    }
    require(
        interactive.wait().await? == Some(0) && echoed == b"go",
        "interactive request did not complete",
    )?;

    let (delayed, mut delayed_output) = sandbox
        .spawn(SpawnRequest {
            program: "cat".into(),
            ..Default::default()
        })
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    delayed.write_stdin(&[0, 255, 3]).await?;
    delayed.close_stdin().await?;
    let mut delayed_bytes = Vec::new();
    while let Some(chunk) = delayed_output.recv().await {
        if !chunk.stderr {
            delayed_bytes.extend(chunk.bytes);
        }
    }
    require(
        delayed.wait().await? == Some(0) && delayed_bytes == [0, 255, 3],
        "process wrapper closed delayed stdin",
    )?;

    sandbox
        .write_file(
            "/workspace/stdin-proof",
            &[0, 255, 3],
            orca_harness_core::FileMode::Regular,
        )
        .await?;
    require(
        sandbox.read_file("/workspace/stdin-proof").await? == [0, 255, 3],
        "process wrapper lost binary stdin",
    )?;

    let (process, mut output) = sandbox
        .spawn(SpawnRequest {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "printf started; (sleep 1; printf late > /workspace/kill-marker) & wait".into(),
            ],
            ..Default::default()
        })
        .await?;
    require(output.recv().await.is_some(), "spawn did not start")?;
    process.kill().await?;
    process.wait().await?;
    let mut request = ExecRequest::new("sleep 1; printf late > /workspace/timeout-marker");
    request.timeout_ms = Some(50);
    require(
        sandbox.exec(request).await.is_err(),
        "exec timeout was not reported",
    )?;
    let cloned = sandbox.clone();
    let task = tokio::spawn(async move {
        cloned
            .exec(ExecRequest::new(
                "touch /workspace/cancel-ready; while [ ! -e /workspace/cancel-go ]; do sleep 0.05; done; sleep 0.2; printf late > /workspace/cancel-marker",
            ))
            .await
    });
    for _ in 0..100 {
        if sandbox.stat("/workspace/cancel-ready").await?.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    require(
        sandbox.stat("/workspace/cancel-ready").await?.is_some(),
        "cancel command did not start",
    )?;
    task.abort();
    let _ = task.await;
    sandbox
        .write_file(
            "/workspace/cancel-go",
            b"go",
            orca_harness_core::FileMode::Regular,
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(1300)).await;
    for path in [
        "/workspace/kill-marker",
        "/workspace/timeout-marker",
        "/workspace/cancel-marker",
    ] {
        require(
            sandbox.stat(path).await?.is_none(),
            &format!("cancelled remote process wrote a late marker: {path}"),
        )?;
    }
    let normal = sandbox
        .exec(ExecRequest::new(
            "head -c 64000000 /dev/zero; printf err >&2; exit 7",
        ))
        .await?;
    require(
        normal.exit_code == 7 && normal.stdout.len() == 64000000 && normal.stderr == b"err",
        "bounded transport changed finite output or exit status",
    )?;
    for command in [
        "head -c 70000000 /dev/zero",
        "head -c 70000000 /dev/zero >&2",
        "head -c 67174401 /dev/zero",
    ] {
        require(
            sandbox.exec(ExecRequest::new(command)).await.is_err(),
            "oversized exec output was accepted",
        )?;
    }
    sandbox
        .exec(ExecRequest::new(
            "truncate -s 70000000 /workspace/large-file",
        ))
        .await?;
    require(
        sandbox.read_file("/workspace/large-file").await.is_err(),
        "oversized file read was accepted",
    )?;
    sandbox
        .write_file(
            "/usr/local/bin/cat",
            b"#!/bin/sh\nhead -c 70000000 /dev/zero >&2\n",
            orca_harness_core::FileMode::Executable,
        )
        .await?;
    require(
        sandbox
            .write_file(
                "/workspace/mock-output",
                b"input",
                orca_harness_core::FileMode::Regular,
            )
            .await
            .is_err(),
        "file write stderr overflow was accepted",
    )?;
    sandbox
        .exec(ExecRequest::new("rm -f /usr/local/bin/cat"))
        .await?;
    let (process, mut output) = sandbox
        .spawn(SpawnRequest {
            program: "sh".into(),
            args: vec!["-c".into(), "head -c 70000000 /dev/zero".into()],
            ..Default::default()
        })
        .await?;
    let mut bytes = 0;
    while let Some(chunk) = output.recv().await {
        bytes += chunk.bytes.len();
    }
    require(
        bytes <= 64 * 1024 * 1024 + 64 * 1024,
        "spawn exceeded its total output bound",
    )?;
    require(
        process.wait().await.is_err(),
        "spawn output overflow was not reported",
    )?;
    require(
        sandbox.exec(ExecRequest::new("printf alive")).await?.stdout == b"alive",
        "sandbox did not survive process cleanup",
    )?;
    Ok(())
}
#[tokio::test]
#[ignore = "explicit local Docker process lifecycle and output-bound proof"]
async fn docker_kills_remote_groups_on_kill_timeout_and_drop_and_bounds_output() {
    let identity = format!(
        "orca-process-test-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let provisioner = DockerProvisioner::new(EnvironmentSpec::new().network(Network::Disabled))
        .named(&identity, &identity);
    let sandbox = provisioner.start().await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(45), scenarios(sandbox.clone())).await;
    let cleanup = sandbox.shutdown().await;
    cleanup.unwrap();
    result.unwrap().unwrap();
}
