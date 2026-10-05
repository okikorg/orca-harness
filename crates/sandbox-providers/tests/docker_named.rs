//! What host-owned named containers promise, checked against a real
//! container: reattach and cleanup honour the owner label, cleanup of an
//! absent container succeeds, and finalize refuses to protect through a
//! symlinked ancestor.
//!
//! Skips where no Docker daemon is available; it needs no API key.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use orca_harness_core::{ExecRequest, Provisioner, Sandbox};
use orca_harness_sandbox_providers::{DockerProvisioner, EnvironmentSpec, Network};

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

async fn proof(identity: &str, root: &Arc<dyn Sandbox>) {
    assert!(
        DockerProvisioner::attach_named(identity, "someone-else", "/workspace")
            .await
            .is_err(),
        "attached without the owner label"
    );
    let attached = DockerProvisioner::attach_named(identity, identity, "/workspace")
        .await
        .expect("attach");
    assert_eq!(run(&attached, "printf ok").await, (0, "ok".into()));

    // A symlinked ancestor must not carry the chown and chmod out of the
    // workspace. A kernel with `fs.protected_symlinks` also stops root from
    // following it here, so the refusal must come from the symlink check.
    let before = run(root, "stat -c %a:%u /etc").await;
    assert_eq!(
        run(root, "ln -s /etc /workspace/.orca").await.0,
        0,
        "could not plant the symlink"
    );
    match DockerProvisioner::finalize_named(
        identity,
        identity,
        "/workspace",
        &["/workspace/.orca/skills".into()],
    )
    .await
    {
        Err(error) => assert!(
            error.to_string().contains("test ! -L '/workspace/.orca'"),
            "refused by something other than the symlink check: {error}"
        ),
        Ok(_) => panic!("finalize protected through a symlinked ancestor"),
    }
    assert_eq!(run(root, "stat -c %a:%u /etc").await, before);
    assert_ne!(run(root, "test -e /etc/skills").await.0, 0);

    // Another owner's cleanup finds nothing to remove.
    DockerProvisioner::cleanup_named(identity, "someone-else")
        .await
        .expect("foreign cleanup");
    assert_eq!(run(&attached, "printf ok").await, (0, "ok".into()));
}

#[tokio::test]
async fn named_containers_honour_their_owner_and_refuse_symlinked_ancestors() {
    if !docker_available().await {
        eprintln!("skipping: no Docker daemon available");
        return;
    }
    let identity = format!(
        "orca-named-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let root = DockerProvisioner::new(EnvironmentSpec::new().network(Network::Disabled))
        .named(&identity, &identity)
        .start()
        .await
        .expect("start");
    let result = tokio::spawn({
        let identity = identity.clone();
        let root = root.clone();
        async move { proof(&identity, &root).await }
    })
    .await;
    let cleanup = DockerProvisioner::cleanup_named(&identity, &identity).await;
    if result.is_err() || cleanup.is_err() {
        let _ = root.shutdown().await;
    }
    result.expect("proof");
    cleanup.expect("cleanup");

    assert!(
        DockerProvisioner::attach_named(&identity, &identity, "/workspace")
            .await
            .is_err(),
        "cleanup left the container behind"
    );
    DockerProvisioner::cleanup_named(&identity, &identity)
        .await
        .expect("cleanup of an absent container");
}
