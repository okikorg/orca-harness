//! Session environment routing, persistence and fail-closed capability coverage.
use async_trait::async_trait;
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    Capabilities, Entry, ExecOutput, ExecRequest, FileMode, ModelResponse, Output, Sandbox,
    SandboxError, Session, SpawnRequest, Stat,
};
use orca_harness_sdk::orchestration::SubagentRequest;
use orca_harness_sdk::{Harness, SessionEnvironment, SubagentConfig, ToolPreset};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
mod common;
#[derive(Default)]
struct MemSandbox {
    caps: Option<Capabilities>,
    fail_exec: bool,
    live_repls: bool,
    hang_first_kill: bool,
    kills: Arc<std::sync::atomic::AtomicUsize>,
    requests: Mutex<Vec<SpawnRequest>>,
    shutdowns: std::sync::atomic::AtomicUsize,
    files: Mutex<HashMap<String, Vec<u8>>>,
    execs: Mutex<Vec<String>>,
}

impl MemSandbox {
    fn get(&self, path: &str) -> Option<Vec<u8>> {
        self.files.lock().unwrap().get(path).cloned()
    }

    fn put(&self, path: &str, bytes: &[u8]) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_string(), bytes.to_vec());
    }
}

#[async_trait]
impl Sandbox for MemSandbox {
    fn capabilities(&self) -> Capabilities {
        self.caps.unwrap_or(Capabilities {
            sessions: true,
            file_api: true,
            network_policy: false,
        })
    }

    async fn exec(&self, request: ExecRequest) -> Result<ExecOutput, SandboxError> {
        self.execs.lock().unwrap().push(request.command.clone());
        if self.fail_exec {
            return Err(SandboxError::Request("provider unavailable".into()));
        }
        // Only `rm -f` reaches here from the file tools.
        if let Some(rest) = request.command.strip_prefix("rm -f ") {
            let path = rest.trim().trim_matches('\'');
            self.files.lock().unwrap().remove(path);
        }
        Ok(ExecOutput::default())
    }

    /// A session that runs nothing: it records the argv it was asked for
    /// and ends at once. Enough to prove `process` reaches the provider
    /// instead of this machine, without a real interpreter in the test.
    async fn spawn(
        &self,
        request: SpawnRequest,
    ) -> Result<(Arc<dyn Session>, Output), SandboxError> {
        self.requests.lock().unwrap().push(request.clone());
        if self.live_repls {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            return Ok((
                Arc::new(LiveMemSession {
                    nonce: if request.program.contains("python") {
                        request.args.last().cloned()
                    } else {
                        None
                    },
                    tx,
                    kills: self.kills.clone(),
                    hang_first_kill: self.hang_first_kill,
                    attempted_kill: std::sync::atomic::AtomicBool::new(false),
                }),
                rx,
            ));
        }
        self.execs
            .lock()
            .unwrap()
            .push(format!("{} {}", request.program, request.args.join(" ")));
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let _ = tx
            .send(orca_harness_core::Chunk {
                stderr: false,
                bytes: b"from the sandbox\n".to_vec(),
            })
            .await;
        drop(tx);
        Ok((Arc::new(MemSession), rx))
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError> {
        self.get(path)
            .ok_or_else(|| SandboxError::Request(format!("no such file: {path}")))
    }

    async fn write_file(&self, path: &str, bytes: &[u8], _: FileMode) -> Result<(), SandboxError> {
        self.put(path, bytes);
        Ok(())
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<Entry>, SandboxError> {
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let mut names: Vec<Entry> = Vec::new();
        for key in self.files.lock().unwrap().keys() {
            let Some(rest) = key.strip_prefix(&prefix) else {
                continue;
            };
            let (name, is_dir) = match rest.split_once('/') {
                Some((head, _)) => (head.to_string(), true),
                None => (rest.to_string(), false),
            };
            if !names.iter().any(|e| e.name == name) {
                names.push(Entry { name, is_dir });
            }
        }
        Ok(names)
    }

    async fn stat(&self, path: &str) -> Result<Option<Stat>, SandboxError> {
        if let Some(bytes) = self.get(path) {
            return Ok(Some(Stat {
                modified: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)),
                len: bytes.len() as u64,
                is_dir: false,
            }));
        }
        // A directory is anything that prefixes a known file.
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let is_dir = self
            .files
            .lock()
            .unwrap()
            .keys()
            .any(|key| key.starts_with(&prefix));
        Ok(is_dir.then_some(Stat {
            modified: None,
            len: 0,
            is_dir: true,
        }))
    }

    async fn shutdown(&self) -> Result<(), SandboxError> {
        self.shutdowns
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

struct MemSession;

#[async_trait]
impl Session for MemSession {
    async fn write_stdin(&self, _: &[u8]) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn close_stdin(&self) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn wait(&self) -> Result<Option<i32>, SandboxError> {
        Ok(Some(0))
    }
    async fn kill(&self) -> Result<(), SandboxError> {
        Ok(())
    }
}

fn environment(id: &str, sandbox: &Arc<MemSandbox>, root: &std::path::Path) -> SessionEnvironment {
    SessionEnvironment::sandbox(id, sandbox.clone(), root).unwrap()
}
fn write_round(id: &str) -> ModelResponse {
    ModelResponse::ToolCalls {
        content: None,
        usage: None,
        calls: vec![call(
            id,
            "write_file",
            json!({"path":"proof.txt","content":id}),
        )],
    }
}

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

/// Keeps its output stream alive and never announces natural process exit.
/// Only protocol replies are emitted, so cleanup must explicitly call kill.
struct LiveMemSession {
    hang_first_kill: bool,
    attempted_kill: std::sync::atomic::AtomicBool,
    nonce: Option<String>,
    tx: tokio::sync::mpsc::Sender<orca_harness_core::Chunk>,
    kills: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl Session for LiveMemSession {
    async fn write_stdin(&self, bytes: &[u8]) -> Result<(), SandboxError> {
        if let Some(nonce) = &self.nonce {
            self.tx
                .send(orca_harness_core::Chunk {
                    stderr: false,
                    bytes: format!("executed\n{nonce} ok\n").into_bytes(),
                })
                .await
                .unwrap();
        } else {
            let input = String::from_utf8_lossy(bytes);
            let token = input
                .split("[\"ORCA\",\"BUN\",\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            self.tx
                .send(orca_harness_core::Chunk {
                    stderr: false,
                    bytes: format!("ORCA_BUN_{token}_OK\n\"ORCA_BUN_{token}_END\"").into_bytes(),
                })
                .await
                .unwrap();
            self.tx
                .send(orca_harness_core::Chunk {
                    stderr: true,
                    bytes: format!("ORCA_BUN_{token}_STDERR_END").into_bytes(),
                })
                .await
                .unwrap();
        }
        Ok(())
    }
    async fn close_stdin(&self) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn wait(&self) -> Result<Option<i32>, SandboxError> {
        std::future::pending().await
    }
    async fn kill(&self) -> Result<(), SandboxError> {
        self.kills.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.hang_first_kill
            && !self
                .attempted_kill
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

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
