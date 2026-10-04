//! The read-before-write guard against a real Docker container: an edit
//! made within the same second as the read must still be seen.
//!
//! Skips where no Docker daemon is available; it needs no API key.

use std::sync::Arc;

use orca_harness_core::{CancellationToken, ExecRequest, Provisioner, Sandbox, Tool, ToolContext};
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

fn ctx(tool: &str) -> ToolContext {
    ToolContext {
        call_id: "test".into(),
        tool_name: tool.into(),
        cancellation: CancellationToken::new(),
        deadline: None,
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
