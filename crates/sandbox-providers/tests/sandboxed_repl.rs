//! `py_kernel` driven through a real sandbox.
//!
//! The REPL tools are the reason `Sandbox::spawn` exists: they keep an
//! interpreter alive across calls and frame writes to its stdin, which a
//! one-shot `exec` cannot express. This proves the whole path — provider
//! session, framed stdin, sentinel-delimited output, state surviving
//! between calls — against a live container rather than a fake.
//!
//! Skips where no Docker daemon is available; it needs no API key.

use std::sync::Arc;

use orca_harness_core::{CancellationToken, Provisioner, Tool, ToolContext};
use orca_harness_sandbox_providers::{DockerProvisioner, EnvironmentSpec};
use orca_harness_tools::PyKernelTool;

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
        tool_name: "py_kernel".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

#[tokio::test]
async fn a_python_kernel_keeps_state_inside_the_sandbox() {
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

    let kernel = Arc::new(PyKernelTool::new().sandbox(sandbox.clone()));

    // State persists across calls — the property that distinguishes a
    // kernel from repeated one-shot execs.
    let first = kernel
        .call(
            serde_json::json!({ "action": "exec", "code": "x = 6 * 7" }),
            &ctx(),
        )
        .await
        .expect("first exec");
    assert_eq!(first["state"], "ok", "got {first:?}");

    let second = kernel
        .call(
            serde_json::json!({ "action": "exec", "code": "print(x)" }),
            &ctx(),
        )
        .await
        .expect("second exec");
    assert_eq!(second["state"], "ok", "got {second:?}");
    assert!(
        second["output"].as_str().unwrap().contains("42"),
        "state did not survive between calls: {second:?}"
    );

    // It really is the sandbox's interpreter, not the host's: the file it
    // writes exists inside and not out here.
    kernel
        .call(
            serde_json::json!({
                "action": "exec",
                "code": "open('/workspace/from-kernel.txt','w').write('inside')"
            }),
            &ctx(),
        )
        .await
        .expect("write from kernel");
    let written = sandbox
        .read_file("/workspace/from-kernel.txt")
        .await
        .expect("the kernel wrote inside the sandbox");
    assert_eq!(written, b"inside");
    assert!(
        !std::path::Path::new("/workspace/from-kernel.txt").exists(),
        "the host filesystem must not have been touched"
    );

    // Errors still come back as tracebacks rather than tool failures.
    let boom = kernel
        .call(
            serde_json::json!({ "action": "exec", "code": "1/0" }),
            &ctx(),
        )
        .await
        .expect("an exception is a result, not a tool error");
    assert_eq!(boom["state"], "error", "got {boom:?}");
    assert!(boom["traceback"]
        .as_str()
        .unwrap()
        .contains("ZeroDivisionError"));

    drop(kernel);
    sandbox.shutdown().await.expect("shutdown");
}
