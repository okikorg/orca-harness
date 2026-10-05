//! Session environment routing, persistence and fail-closed capability coverage.
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{Capabilities, ModelResponse};
use orca_harness_sdk::orchestration::SubagentRequest;
use orca_harness_sdk::{Harness, SessionEnvironment, SubagentConfig, ToolPreset};
use serde_json::json;
use std::sync::Arc;

mod common;
mod environment_support;
use environment_support::{environment, write_round, MemSandbox};

#[tokio::test]
async fn sessions_and_children_route_files_to_their_own_sandbox() {
    let root = common::temp_dir("environment-files");
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let model = ScriptedModel::new(vec![
        write_round("a"),
        ModelResponse::final_text("a"),
        write_round("b"),
        ModelResponse::final_text("b"),
        ModelResponse::ToolCalls {
            content: None,
            usage: None,
            calls: vec![call(
                "child",
                "write_file",
                json!({"path":"child.txt","content":"child"}),
            )],
        },
        ModelResponse::final_text("child"),
    ]);
    let agent = harness
        .agent(model)
        .tools(ToolPreset::Coding)
        .subagents(SubagentConfig::default())
        .build()
        .unwrap();
    let a = Arc::new(MemSandbox::default());
    let b = Arc::new(MemSandbox::default());
    let sa = agent
        .new_session()
        .environment(environment("a", &a, &root))
        .open()
        .unwrap();
    let sb = agent
        .new_session()
        .environment(environment("b", &b, &root))
        .open()
        .unwrap();
    sa.run("write").await.unwrap();
    sb.run("write").await.unwrap();
    sa.subagents()
        .unwrap()
        .run(SubagentRequest::new("write"), None, None)
        .await
        .unwrap();
    let key = root.join("proof.txt").display().to_string();
    assert_eq!(a.get(&key).unwrap(), b"a");
    assert_eq!(b.get(&key).unwrap(), b"b");
    assert_eq!(
        a.get(&root.join("child.txt").display().to_string())
            .unwrap(),
        b"child"
    );
    assert!(b
        .get(&root.join("child.txt").display().to_string())
        .is_none());
    assert!(!root.join("proof.txt").exists());
    assert!(!root.join("child.txt").exists());
    sa.shutdown(std::time::Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(a.shutdowns.load(std::sync::atomic::Ordering::SeqCst), 0);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn persistent_binding_survives_resume_fork_and_clear() {
    let root = common::temp_dir("environment-persistence");
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let agent = harness
        .agent(ScriptedModel::new(vec![]))
        .tools(ToolPreset::ReadOnly)
        .build()
        .unwrap();
    let sandbox = Arc::new(MemSandbox::default());
    let env = environment("original", &sandbox, &root);
    let session = agent
        .new_session()
        .persistent()
        .environment(env.clone())
        .open()
        .unwrap();
    let id = session.id().unwrap();
    assert!(agent.resume_session(&id).is_err());
    assert!(agent
        .resume_session_with_environment(&id, environment("wrong", &sandbox, &root))
        .is_err());
    assert!(agent
        .resume_session_with_environment(
            &id,
            environment("original", &sandbox, &root.join("other"))
        )
        .is_err());
    let resumed = agent
        .resume_session_with_environment(&id, env.clone())
        .unwrap();
    let fork = resumed.fork().await.unwrap();
    assert!(agent.resume_session(&fork.id().unwrap()).is_err());
    agent
        .resume_session_with_environment(&fork.id().unwrap(), env.clone())
        .unwrap();
    resumed.clear().await.unwrap();
    assert!(agent.resume_session(&resumed.id().unwrap()).is_err());
    agent
        .resume_session_with_environment(&resumed.id().unwrap(), env.clone())
        .unwrap();
    resumed.reset_in_place().await.unwrap();
    agent
        .resume_session_with_environment(&resumed.id().unwrap(), env.clone())
        .unwrap();
    let sidecar = resumed.path().unwrap().with_extension("environment.json");
    std::fs::write(&sidecar, b"broken").unwrap();
    assert!(agent
        .resume_session_with_environment(&resumed.id().unwrap(), env.clone())
        .is_err());
    assert!(agent.resume_session(&resumed.id().unwrap()).is_err());
    std::fs::remove_file(&sidecar).unwrap();
    assert!(agent
        .resume_session_with_environment(&resumed.id().unwrap(), env)
        .is_err());
    assert!(agent.resume_session(&resumed.id().unwrap()).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn capabilities_are_validated_before_session_creation() {
    let root = common::temp_dir("environment-caps");
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    for (preset, python, bun, caps) in [
        (ToolPreset::ReadOnly, false, false, Capabilities::default()),
        (
            ToolPreset::Coding,
            false,
            false,
            Capabilities {
                file_api: true,
                ..Default::default()
            },
        ),
        (ToolPreset::None, true, false, Capabilities::default()),
        (ToolPreset::None, false, true, Capabilities::default()),
    ] {
        let mut builder = harness.agent(ScriptedModel::new(vec![])).tools(preset);
        if python {
            builder = builder.python();
        }
        if bun {
            builder = builder.bun();
        }
        let agent = builder.build().unwrap();
        let sandbox = Arc::new(MemSandbox {
            caps: Some(caps),
            ..Default::default()
        });
        assert!(agent
            .new_session()
            .persistent()
            .environment(environment("caps", &sandbox, &root))
            .open()
            .is_err());
    }
    assert!(harness.sessions().list().is_empty());
    assert!(
        SessionEnvironment::sandbox("id", Arc::new(MemSandbox::default()), "relative").is_err()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn disabled_environment_has_no_files_commands_or_repls() {
    let root = common::temp_dir("environment-disabled");
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let agent = harness
        .agent(ScriptedModel::new(vec![
            write_round("blocked"),
            ModelResponse::final_text("done"),
        ]))
        .tools(ToolPreset::Coding)
        .python()
        .bun()
        .build()
        .unwrap();
    let session = agent
        .new_session()
        .environment(SessionEnvironment::Disabled)
        .open()
        .unwrap();
    assert!(session.processes().is_none());
    let _ = session.run("write").await;
    assert!(!root.join("proof.txt").exists());
    let transcript = format!("{:?}", session.messages().await);
    assert!(
        transcript.contains("unknown tool") || transcript.contains("not found"),
        "{transcript}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn command_and_repl_launches_use_sandbox_cwd() {
    let root = common::temp_dir("environment-spawn");
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let agent = harness
        .agent(ScriptedModel::tool_round(
            vec![
                call("s", "shell", json!({"command":"echo boundary"})),
                call("p", "pykernel", json!({"code":"print(1)"})),
                call("b", "bun_repl", json!({"code":"console.log(1)"})),
            ],
            "done",
        ))
        .tools(ToolPreset::Coding)
        .python()
        .bun()
        .build()
        .unwrap();
    let sandbox = Arc::new(MemSandbox::default());
    let session = agent
        .new_session()
        .environment(environment("spawn", &sandbox, &root))
        .open()
        .unwrap();
    session.run("run").await.unwrap();
    session
        .processes()
        .unwrap()
        .spawn(
            orca_harness_sdk::orchestration::ProcessSpawn::new("echo process-boundary"),
            None,
        )
        .await
        .unwrap();
    let requests = sandbox.requests.lock().unwrap();
    assert!(
        requests.iter().any(|r| r.program.contains("python")),
        "{requests:?}"
    );
    assert!(
        requests
            .iter()
            .any(|r| r.args.iter().any(|a| a.contains("process-boundary"))),
        "{requests:?}"
    );
    assert!(requests.iter().any(|r| r.program == "bun"), "{requests:?}");
    assert!(requests.iter().all(|r| r.cwd.as_deref() == root.to_str()));
    assert!(sandbox
        .execs
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.contains("echo boundary")));
    drop(requests);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn provider_failure_does_not_retry_on_host() {
    let root = common::temp_dir("environment-no-fallback");
    let sentinel = root.join("must-not-exist");
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let agent = harness
        .agent(ScriptedModel::tool_round(
            vec![call(
                "shell",
                "shell",
                json!({"command":format!("touch '{}'", sentinel.display())}),
            )],
            "done",
        ))
        .tools(ToolPreset::Coding)
        .build()
        .unwrap();
    let sandbox = Arc::new(MemSandbox {
        fail_exec: true,
        ..Default::default()
    });
    let session = agent
        .new_session()
        .environment(environment("unavailable", &sandbox, &root))
        .open()
        .unwrap();
    session.run("execute").await.unwrap();
    assert_eq!(sandbox.execs.lock().unwrap().len(), 1);
    assert!(!sentinel.exists());
    assert!(format!("{:?}", session.messages().await).contains("provider unavailable"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn conflicting_process_recipe_and_bun_without_file_api_fail_early() {
    let root = common::temp_dir("environment-conflicts");
    let harness = Harness::builder()
        .workspace(&root)
        .state_dir(root.join("state"))
        .build()
        .unwrap();
    let sandbox = Arc::new(MemSandbox::default());
    let agent = harness
        .agent(ScriptedModel::new(vec![]))
        .tools(ToolPreset::Coding)
        .processes(
            orca_harness_sdk::ProcessConfig::new().executor(orca_harness_sdk::Executor::local_sh()),
        )
        .build()
        .unwrap();
    assert!(agent
        .new_session()
        .environment(environment("conflict", &sandbox, &root))
        .open()
        .is_err());
    let sandbox = Arc::new(MemSandbox {
        caps: Some(Capabilities {
            sessions: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    let agent = harness
        .agent(ScriptedModel::new(vec![]))
        .bun()
        .build()
        .unwrap();
    assert!(agent
        .new_session()
        .environment(environment("bun", &sandbox, &root))
        .open()
        .is_err());
    std::fs::remove_dir_all(root).unwrap();
}
