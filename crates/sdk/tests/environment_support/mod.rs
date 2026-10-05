//! The in-memory sandbox the session environment suites run against: a
//! file map, recorded execs and spawns, and REPL sessions that either exit
//! at once or stay live until killed.
#![allow(dead_code)]

use async_trait::async_trait;
use orca_harness_core::testing::call;
use orca_harness_core::{
    Capabilities, Entry, ExecOutput, ExecRequest, FileMode, ModelResponse, Output, Sandbox,
    SandboxError, Session, SpawnRequest, Stat,
};
use orca_harness_sdk::SessionEnvironment;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub struct MemSandbox {
    pub caps: Option<Capabilities>,
    pub fail_exec: bool,
    pub live_repls: bool,
    pub hang_first_kill: bool,
    pub kills: Arc<std::sync::atomic::AtomicUsize>,
    pub requests: Mutex<Vec<SpawnRequest>>,
    pub shutdowns: std::sync::atomic::AtomicUsize,
    pub files: Mutex<HashMap<String, Vec<u8>>>,
    pub execs: Mutex<Vec<String>>,
}

impl MemSandbox {
    pub fn get(&self, path: &str) -> Option<Vec<u8>> {
        self.files.lock().unwrap().get(path).cloned()
    }

    pub fn put(&self, path: &str, bytes: &[u8]) {
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

pub struct MemSession;

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

pub fn environment(
    id: &str,
    sandbox: &Arc<MemSandbox>,
    root: &std::path::Path,
) -> SessionEnvironment {
    SessionEnvironment::sandbox(id, sandbox.clone(), root).unwrap()
}
pub fn write_round(id: &str) -> ModelResponse {
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

/// Keeps its output stream alive and never announces natural process exit.
/// Only protocol replies are emitted, so cleanup must explicitly call kill.
pub struct LiveMemSession {
    pub hang_first_kill: bool,
    pub attempted_kill: std::sync::atomic::AtomicBool,
    pub nonce: Option<String>,
    pub tx: tokio::sync::mpsc::Sender<orca_harness_core::Chunk>,
    pub kills: Arc<std::sync::atomic::AtomicUsize>,
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
