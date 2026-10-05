//! Session environment REPL lifecycle: clear, reset, shutdown and drop kill
//! the remote interpreters without destroying the sandbox they ran in.
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_sdk::Harness;
use serde_json::json;
use std::sync::Arc;

mod common;
mod environment_support;
use environment_support::{environment, MemSandbox};

#[tokio::test]
async fn lifecycle_kills_remote_python_and_bun_without_destroying_sandbox() {
    use std::sync::atomic::Ordering;
    for action in ["clear", "reset", "shutdown", "drop"] {
        let root = common::temp_dir(action);
        let harness = Harness::builder()
            .workspace(&root)
            .state_dir(root.join("state"))
            .build()
            .unwrap();
        let agent = harness
            .agent(ScriptedModel::tool_round(
                vec![
                    call(
                        "python",
                        "pykernel",
                        json!({"code":"print(1)", "timeoutMs":100}),
                    ),
                    call(
                        "bun",
                        "bun_repl",
                        json!({"code":"console.log(1)", "timeoutMs":100}),
                    ),
                ],
                "done",
            ))
            .python()
            .bun()
            .build()
            .unwrap();
        let sandbox = Arc::new(MemSandbox {
            live_repls: true,
            ..Default::default()
        });
        let session = agent
            .new_session()
            .persistent()
            .environment(environment("live", &sandbox, &root))
            .open()
            .unwrap();
        session.run("start interpreters").await.unwrap();
        assert_eq!(sandbox.requests.lock().unwrap().len(), 2);
        assert_eq!(
            sandbox.kills.load(Ordering::SeqCst),
            0,
            "interpreters must remain live before {action}"
        );
        match action {
            "clear" => session.clear().await.unwrap(),
            "reset" => session.reset_in_place().await.unwrap(),
            "shutdown" => session
                .shutdown(std::time::Duration::from_secs(1))
                .await
                .unwrap(),
            "drop" => drop(session),
            _ => unreachable!(),
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while sandbox.kills.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(sandbox.kills.load(Ordering::SeqCst), 2, "{action}");
        assert_eq!(sandbox.shutdowns.load(Ordering::SeqCst), 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn shutdown_deadline_starts_all_repl_kills_and_drop_can_retry() {
    use std::sync::atomic::Ordering;
    let root = common::temp_dir("repl-kill-deadline");
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let agent = harness
        .agent(ScriptedModel::tool_round(
            vec![
                call(
                    "python",
                    "pykernel",
                    json!({"code":"print(1)", "timeoutMs":100}),
                ),
                call(
                    "bun",
                    "bun_repl",
                    json!({"code":"console.log(1)", "timeoutMs":100}),
                ),
            ],
            "done",
        ))
        .python()
        .bun()
        .build()
        .unwrap();
    let sandbox = Arc::new(MemSandbox {
        live_repls: true,
        hang_first_kill: true,
        ..Default::default()
    });
    let session = agent
        .new_session()
        .environment(environment("live", &sandbox, &root))
        .open()
        .unwrap();
    session.run("start").await.unwrap();
    let result = session.shutdown(std::time::Duration::from_millis(20)).await;
    assert!(matches!(
        result,
        Err(orca_harness_sdk::SdkError::ShutdownTimeout { .. })
    ));
    assert_eq!(
        sandbox.kills.load(Ordering::SeqCst),
        2,
        "both interpreters must receive kill before timeout"
    );
    drop(session);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while sandbox.kills.load(Ordering::SeqCst) < 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(sandbox.kills.load(Ordering::SeqCst), 4);
    assert_eq!(sandbox.shutdowns.load(Ordering::SeqCst), 0);
    std::fs::remove_dir_all(root).unwrap();
}
