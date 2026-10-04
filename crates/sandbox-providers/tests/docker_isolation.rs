//! What the Docker adapter promises about isolation, checked against a real
//! container: a killed or timed-out command stops inside it, capability
//! directories cannot be changed by the agent, a failed or abandoned
//! provisioning leaves no container behind, and the read-before-write guard
//! sees an edit made within the same second.
//!
//! Skips where no Docker daemon is available; it needs no API key.

use std::sync::Arc;
use std::time::Duration;

use orca_harness_core::{
    CancellationToken, ExecRequest, FileMode, Provisioner, Sandbox, SpawnRequest, Tool, ToolContext,
};
use orca_harness_sandbox_providers::{DockerProvisioner, EnvironmentSpec};
use orca_harness_tools::{FileGuard, ReadFileTool, Workspace, WriteFileTool};

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

async fn run(sandbox: &Arc<dyn Sandbox>, command: &str) -> (i32, String) {
    let output = sandbox.exec(ExecRequest::new(command)).await.expect("exec");
    (
        output.exit_code,
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    )
}

/// Exit 0 when the PID recorded in `file` is gone (or only a zombie).
fn gone(file: &str) -> String {
    format!("pid=$(cat {file}); [ ! -r /proc/$pid/stat ] || grep -q ') Z ' /proc/$pid/stat")
}

/// Container ids whose environment carries `marker`.
async fn containers_marked(marker: &str) -> Vec<String> {
    let listed = tokio::process::Command::new("docker")
        .args(["ps", "-aq", "--no-trunc"])
        .output()
        .await
        .expect("docker ps");
    let mut found = Vec::new();
    for id in String::from_utf8_lossy(&listed.stdout).split_whitespace() {
        let env = tokio::process::Command::new("docker")
            .args([
                "inspect",
                "-f",
                "{{range .Config.Env}}{{println .}}{{end}}",
                id,
            ])
            .output()
            .await
            .expect("docker inspect");
        if String::from_utf8_lossy(&env.stdout)
            .lines()
            .any(|line| line == marker)
        {
            found.push(id.to_string());
        }
    }
    found
}

fn marker(name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("ORCA_TEST_MARKER={name}-{}-{nanos}", std::process::id())
}

fn ctx(tool: &str) -> ToolContext {
    ToolContext {
        call_id: "test".into(),
        tool_name: tool.into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

#[tokio::test]
async fn killed_and_timed_out_commands_stop_inside_the_container() {
    if !docker_available().await {
        eprintln!("skipping: no Docker daemon available");
        return;
    }
    let sandbox = DockerProvisioner::new(EnvironmentSpec::new())
        .start()
        .await
        .expect("start");

    // A session kill must end the process in the container, not just the
    // `docker exec` client on the host.
    let (session, _output) = sandbox
        .spawn(SpawnRequest {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "echo $$ > /workspace/spawned.pid; exec sleep 300".into(),
            ],
            ..Default::default()
        })
        .await
        .expect("spawn");
    for _ in 0..100 {
        if run(&sandbox, "test -s /workspace/spawned.pid").await.0 == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    session.kill().await.expect("kill");
    assert_eq!(
        run(&sandbox, &gone("/workspace/spawned.pid")).await.0,
        0,
        "the killed process is still running inside the container"
    );

    // Likewise for a command that outlives its timeout.
    let mut request = ExecRequest::new("echo $$ > /workspace/timed.pid; exec sleep 300");
    request.timeout_ms = Some(500);
    assert!(sandbox.exec(request).await.is_err(), "timeout not reported");
    assert_eq!(
        run(&sandbox, &gone("/workspace/timed.pid")).await.0,
        0,
        "the timed-out process is still running inside the container"
    );

    sandbox.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn capability_directories_cannot_be_changed_by_agent_commands() {
    if !docker_available().await {
        eprintln!("skipping: no Docker daemon available");
        return;
    }
    let sandbox = DockerProvisioner::new(
        EnvironmentSpec::new()
            .setup_command(
                "mkdir -p /workspace/skills && printf original > /workspace/skills/tool.md",
                None,
            )
            .capability_directories(["/workspace/skills"]),
    )
    .start()
    .await
    .expect("start");

    for attempt in [
        "printf changed > /workspace/skills/tool.md",
        "chmod -R u+w /workspace/skills",
        "rm -f /workspace/skills/tool.md",
        "mv /workspace/skills /workspace/moved",
        "rm -rf /workspace/skills",
    ] {
        assert_ne!(run(&sandbox, attempt).await.0, 0, "allowed: {attempt}");
    }
    assert!(
        sandbox
            .write_file("/workspace/skills/tool.md", b"changed", FileMode::Regular)
            .await
            .is_err(),
        "write_file overwrote a capability file"
    );
    assert_eq!(
        run(&sandbox, "cat /workspace/skills/tool.md").await,
        (0, "original".into())
    );

    // The rest of the workspace is still the agent's.
    assert_eq!(
        run(&sandbox, "printf ok > /workspace/out && cat /workspace/out").await,
        (0, "ok".into())
    );

    sandbox.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn failed_provisioning_leaves_no_container() {
    if !docker_available().await {
        eprintln!("skipping: no Docker daemon available");
        return;
    }
    let marker = marker("failed-setup");
    let (key, value) = marker.split_once('=').unwrap();
    let started = DockerProvisioner::new(
        EnvironmentSpec::new()
            .env([(key, value)])
            .setup_command("exit 7", None),
    )
    .start()
    .await;
    assert!(started.is_err(), "a failing setup command must fail start");
    assert!(
        containers_marked(&marker).await.is_empty(),
        "the container from the failed provisioning is still there"
    );
}

#[tokio::test]
async fn abandoned_provisioning_leaves_no_container() {
    if !docker_available().await {
        eprintln!("skipping: no Docker daemon available");
        return;
    }
    let marker = marker("abandoned");
    let (key, value) = marker.split_once('=').unwrap();
    let provisioner = DockerProvisioner::new(
        EnvironmentSpec::new()
            .env([(key, value)])
            .setup_command("sleep 60", None),
    );
    let start = tokio::spawn(async move { provisioner.start().await.map(|_| ()) });

    // Cancel only once the container exists and setup is underway.
    let mut seen = false;
    for _ in 0..200 {
        if !containers_marked(&marker).await.is_empty() {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(seen, "the container never started");
    start.abort();
    let _ = start.await;

    for _ in 0..100 {
        if containers_marked(&marker).await.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the abandoned container was not removed");
}

#[tokio::test]
async fn provisioning_cancelled_during_docker_run_leaves_no_container() {
    if !docker_available().await {
        eprintln!("skipping: no Docker daemon available");
        return;
    }
    // Cancel at several points across `docker run` itself, before the
    // adapter has a container id to clean up by.
    for delay_ms in [0, 20, 50, 100, 200, 400] {
        let marker = marker(&format!("cancel-run-{delay_ms}"));
        let (key, value) = marker.split_once('=').unwrap();
        let provisioner = DockerProvisioner::new(EnvironmentSpec::new().env([(key, value)]));
        let start = tokio::spawn(async move { provisioner.start().await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        start.abort();
        let finished = start.await;

        if matches!(finished, Ok(Ok(()))) {
            // It won the race: a sandbox the caller dropped is the
            // caller's to shut down, so remove it here.
            for id in containers_marked(&marker).await {
                let _ = tokio::process::Command::new("docker")
                    .args(["rm", "-f", &id])
                    .output()
                    .await;
            }
            continue;
        }
        let mut removed = false;
        for _ in 0..150 {
            if containers_marked(&marker).await.is_empty() {
                removed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            removed,
            "cancelled after {delay_ms}ms: the container was left behind"
        );
    }
}

#[tokio::test]
async fn an_edit_within_the_same_second_is_seen_by_the_guard() {
    if !docker_available().await {
        eprintln!("skipping: no Docker daemon available");
        return;
    }
    let sandbox = DockerProvisioner::new(EnvironmentSpec::new())
        .start()
        .await
        .expect("start");
    let ws = Workspace::sandboxed("/workspace", sandbox.clone());
    let guard = FileGuard::new();

    // Pin both modification times inside one second, so only the
    // sub-second part tells the two versions apart.
    assert_eq!(
        run(
            &sandbox,
            "printf aaaaa > /workspace/f.txt && touch -d @1757635200.1 /workspace/f.txt"
        )
        .await
        .0,
        0
    );
    ReadFileTool::new(ws.clone())
        .guard(guard.clone())
        .call(serde_json::json!({ "path": "f.txt" }), &ctx("read_file"))
        .await
        .expect("read");
    assert_eq!(
        run(
            &sandbox,
            "printf bbbbb > /workspace/f.txt && touch -d @1757635200.5 /workspace/f.txt"
        )
        .await
        .0,
        0
    );

    let refused = WriteFileTool::new(ws)
        .guard(guard)
        .call(
            serde_json::json!({ "path": "f.txt", "content": "ccccc" }),
            &ctx("write_file"),
        )
        .await;
    assert!(refused.is_err(), "overwrote an edit the agent never read");
    assert_eq!(
        run(&sandbox, "cat /workspace/f.txt").await,
        (0, "bbbbb".into())
    );

    sandbox.shutdown().await.expect("shutdown");
}
