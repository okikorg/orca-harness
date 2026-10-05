//! Unsupported remote EOF must not be reported as success or lose writable stdin.
use async_trait::async_trait;
use orca_harness_core::{
    CancellationToken, Capabilities, Chunk, Entry, ExecOutput, ExecRequest, FileMode, Output,
    Sandbox, SandboxError, Session, SpawnRequest, Stat,
};
use orca_harness_tools::{Executor, ProcessSpawn, ProcessTool, ProcessWrite};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::sync::mpsc;
#[derive(Default)]
struct Remote {
    eof_supported: AtomicBool,
    written: Mutex<Vec<u8>>,
    sender: Mutex<Option<mpsc::Sender<Chunk>>>,
    done: CancellationToken,
}
#[async_trait]
impl Session for Remote {
    async fn write_stdin(&self, bytes: &[u8]) -> Result<(), SandboxError> {
        self.written.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
    async fn close_stdin(&self) -> Result<(), SandboxError> {
        if !self.eof_supported.load(Ordering::SeqCst) {
            return Err(SandboxError::Unsupported {
                provider: "recording",
                capability: "closing active process stdin",
            });
        }
        self.sender.lock().unwrap().take();
        self.done.cancel();
        Ok(())
    }
    async fn wait(&self) -> Result<Option<i32>, SandboxError> {
        self.done.cancelled().await;
        Ok(Some(0))
    }
    async fn kill(&self) -> Result<(), SandboxError> {
        self.sender.lock().unwrap().take();
        self.done.cancel();
        Ok(())
    }
}
struct Boundary(Arc<Remote>);
#[async_trait]
impl Sandbox for Boundary {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            sessions: true,
            ..Default::default()
        }
    }
    async fn spawn(&self, _: SpawnRequest) -> Result<(Arc<dyn Session>, Output), SandboxError> {
        let (tx, rx) = mpsc::channel(2);
        *self.0.sender.lock().unwrap() = Some(tx);
        Ok((self.0.clone(), rx))
    }
    async fn exec(&self, _: ExecRequest) -> Result<ExecOutput, SandboxError> {
        unreachable!()
    }
    async fn read_file(&self, _: &str) -> Result<Vec<u8>, SandboxError> {
        unreachable!()
    }
    async fn write_file(&self, _: &str, _: &[u8], _: FileMode) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn list_dir(&self, _: &str) -> Result<Vec<Entry>, SandboxError> {
        unreachable!()
    }
    async fn stat(&self, _: &str) -> Result<Option<Stat>, SandboxError> {
        unreachable!()
    }
    async fn shutdown(&self) -> Result<(), SandboxError> {
        unreachable!()
    }
}
#[tokio::test]
async fn unsupported_eof_preserves_stdin_for_later_write_and_successful_retry() {
    let remote = Arc::new(Remote::default());
    let tool = ProcessTool::new(Executor::sandbox(Arc::new(Boundary(remote.clone()))));
    let controller = tool.controller();
    let id = controller
        .spawn(ProcessSpawn::new("remote-only"), CancellationToken::new())
        .await
        .unwrap()
        .id;
    let error = controller
        .write(
            &id,
            ProcessWrite::new("").newline(false).eof(true),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("closing active process stdin"));
    assert!(!error.to_string().contains("input written"));
    let snapshot = controller
        .write(
            &id,
            ProcessWrite::new("still writable").newline(false),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(snapshot.running);
    assert_eq!(*remote.written.lock().unwrap(), b"still writable");
    remote.eof_supported.store(true, Ordering::SeqCst);
    let snapshot = controller
        .write(
            &id,
            ProcessWrite::new("").newline(false).eof(true),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!snapshot.running);
    assert_eq!(snapshot.exit_code, Some(0));
    controller.kill(&id).await.unwrap();
}
#[tokio::test]
async fn failed_eof_after_input_reports_the_input_as_written() {
    let remote = Arc::new(Remote::default());
    let tool = ProcessTool::new(Executor::sandbox(Arc::new(Boundary(remote.clone()))));
    let controller = tool.controller();
    let id = controller
        .spawn(ProcessSpawn::new("remote-only"), CancellationToken::new())
        .await
        .unwrap()
        .id;
    let error = controller
        .write(
            &id,
            ProcessWrite::new("payload").newline(false).eof(true),
            CancellationToken::new(),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("input written, but closing stdin failed: "),
        "{error}"
    );
    assert!(error.contains("closing active process stdin"), "{error}");
    assert_eq!(*remote.written.lock().unwrap(), b"payload");
    remote.eof_supported.store(true, Ordering::SeqCst);
    let snapshot = controller
        .write(
            &id,
            ProcessWrite::new("").newline(false).eof(true),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!snapshot.running);
    assert_eq!(*remote.written.lock().unwrap(), b"payload");
    controller.kill(&id).await.unwrap();
}
