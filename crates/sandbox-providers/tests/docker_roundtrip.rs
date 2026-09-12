//! End-to-end exercise of the trait against a real sandbox, with no API
//! key: the Docker adapter is the reason the sandbox path is testable at
//! all. Skips (rather than fails) where no Docker daemon is available, so
//! the default `cargo test` run stays green on a laptop and in CI.

use orca_harness_core::{ExecRequest, FileMode, Provisioner};
use orca_harness_sandbox_providers::{DockerProvisioner, EnvironmentSpec};

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

#[tokio::test]
async fn exec_files_and_a_live_session_round_trip() {
    if !docker_available().await {
        eprintln!("skipping: no Docker daemon available");
        return;
    }

    let sandbox = DockerProvisioner::new(EnvironmentSpec::new().workspace_directory("/workspace"))
        .start()
        .await
        .expect("start a docker sandbox");

    // exec: the command runs inside, and the working directory is the
    // declared workspace rather than wherever the host happened to be.
    let out = sandbox
        .exec(ExecRequest::new("pwd && echo marker"))
        .await
        .expect("exec");
    assert_eq!(out.exit_code, 0);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("/workspace"), "got {stdout:?}");
    assert!(stdout.contains("marker"));

    // A non-zero exit is reported, not swallowed.
    let failed = sandbox
        .exec(ExecRequest::new("exit 3"))
        .await
        .expect("exec");
    assert_eq!(failed.exit_code, 3);

    // Files: bytes survive the round trip, including a quote that would
    // break naive shell quoting.
    let content = b"it's binary-ish \x00\xff and multi\nline";
    sandbox
        .write_file("/workspace/sub dir/a'b.txt", content, FileMode::Regular)
        .await
        .expect("write");
    let read = sandbox
        .read_file("/workspace/sub dir/a'b.txt")
        .await
        .expect("read");
    assert_eq!(read, content);

    let entries = sandbox.list_dir("/workspace").await.expect("list");
    assert!(entries.iter().any(|e| e.name == "sub dir" && e.is_dir));

    // A live session with stdin — the capability the REPL tools need and
    // the one that separates providers that can host an enclosure from
    // those that cannot.
    assert!(sandbox.capabilities().sessions);
    let (session, mut output) = sandbox
        .spawn(orca_harness_core::SpawnRequest {
            program: "sh".into(),
            args: vec![],
            ..Default::default()
        })
        .await
        .expect("spawn");

    session
        .write_stdin(b"echo one\n")
        .await
        .expect("write stdin");
    session
        .write_stdin(b"echo two\nexit\n")
        .await
        .expect("write stdin again");

    let mut seen = String::new();
    while let Ok(Some(chunk)) =
        tokio::time::timeout(std::time::Duration::from_secs(10), output.recv()).await
    {
        seen.push_str(&String::from_utf8_lossy(&chunk.bytes));
        if seen.contains("two") {
            break;
        }
    }
    assert!(seen.contains("one"), "got {seen:?}");
    assert!(seen.contains("two"), "got {seen:?}");

    session.kill().await.ok();
    sandbox.shutdown().await.expect("shutdown");
}
