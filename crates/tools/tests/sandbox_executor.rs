//! `shell` against a sandbox executor.
//!
//! The property under test is not "it produces output" but "it does not
//! touch the host": a sandboxed shell that quietly spawns a local process
//! is the exact failure the sandbox boundary exists to prevent, and it
//! would otherwise look identical from the outside.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use orca_harness_core::{
    CancellationToken, Capabilities, Entry, ExecOutput, ExecRequest, FileMode, Output, Sandbox,
    SandboxError, Session, SpawnRequest, Stat, Tool, ToolContext,
};
use orca_harness_tools::{Executor, ShellTool};

#[derive(Default)]
struct FakeSandbox {
    commands: Mutex<Vec<ExecRequest>>,
    execs: AtomicUsize,
}

#[async_trait]
impl Sandbox for FakeSandbox {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            sessions: true,
            file_api: true,
            network_policy: false,
        }
    }

    async fn exec(&self, request: ExecRequest) -> Result<ExecOutput, SandboxError> {
        self.execs.fetch_add(1, Ordering::SeqCst);
        self.commands.lock().unwrap().push(request);
        Ok(ExecOutput {
            stdout: b"from the sandbox".to_vec(),
            stderr: b"a warning".to_vec(),
            exit_code: 0,
        })
    }

    async fn spawn(&self, _: SpawnRequest) -> Result<(Arc<dyn Session>, Output), SandboxError> {
        Err(SandboxError::Unsupported {
            provider: "fake",
            capability: "sessions",
        })
    }

    async fn read_file(&self, _: &str) -> Result<Vec<u8>, SandboxError> {
        Ok(Vec::new())
    }

    async fn write_file(&self, _: &str, _: &[u8], _: FileMode) -> Result<(), SandboxError> {
        Ok(())
    }

    async fn list_dir(&self, _: &str) -> Result<Vec<Entry>, SandboxError> {
        Ok(Vec::new())
    }

    async fn stat(&self, _: &str) -> Result<Option<Stat>, SandboxError> {
        Ok(None)
    }

    async fn shutdown(&self) -> Result<(), SandboxError> {
        Ok(())
    }
}

fn ctx(cancellation: CancellationToken) -> ToolContext {
    ToolContext {
        call_id: "test".into(),
        tool_name: "shell".into(),
        cancellation,
        deadline: None,
    }
}

#[tokio::test]
async fn routes_to_the_sandbox_and_never_to_the_host() {
    let sandbox = Arc::new(FakeSandbox::default());
    let tool = ShellTool::new(Executor::sandbox(sandbox.clone())).working_dir("/workspace");

    // A command that would be unmistakable if it ran locally: `id -u` on
    // the host prints a real uid, and nothing here should reach a shell.
    let out = tool
        .call(
            serde_json::json!({ "command": "id -u" }),
            &ctx(CancellationToken::new()),
        )
        .await
        .expect("sandbox shell call");

    assert_eq!(out["stdout"], "from the sandbox");
    assert_eq!(out["stderr"], "a warning");
    assert_eq!(out["exitCode"], 0);
    assert_eq!(out["success"], true);

    // The sandbox saw it, exactly once, with the working directory
    // carried through rather than dropped.
    assert_eq!(sandbox.execs.load(Ordering::SeqCst), 1);
    let seen = sandbox.commands.lock().unwrap();
    assert_eq!(seen[0].command, "id -u");
    assert_eq!(seen[0].cwd.as_deref(), Some("/workspace"));
}

#[tokio::test]
async fn cancellation_still_wins() {
    let sandbox = Arc::new(FakeSandbox::default());
    let tool = ShellTool::new(Executor::sandbox(sandbox.clone()));

    let token = CancellationToken::new();
    token.cancel();
    let error = tool
        .call(serde_json::json!({ "command": "sleep 60" }), &ctx(token))
        .await
        .expect_err("a cancelled call must not run");

    assert!(error.to_string().contains("cancelled"));
    assert_eq!(
        sandbox.execs.load(Ordering::SeqCst),
        0,
        "a cancelled call must not reach the provider"
    );
}

#[tokio::test]
async fn empty_command_is_rejected_before_the_provider_is_called() {
    let sandbox = Arc::new(FakeSandbox::default());
    let tool = ShellTool::new(Executor::sandbox(sandbox.clone()));

    let error = tool
        .call(
            serde_json::json!({ "command": "   " }),
            &ctx(CancellationToken::new()),
        )
        .await
        .expect_err("empty commands are rejected");

    assert!(error.to_string().contains("must not be empty"));
    assert_eq!(sandbox.execs.load(Ordering::SeqCst), 0);
}

#[test]
fn existing_executor_constructors_are_unchanged() {
    // The refactor's contract: every previously public way to build an
    // Executor still exists with the same shape, so no call site churns.
    let _ = Executor::local_sh();
    let _ = Executor::new("ssh", ["user@host"]);
    let _ = Executor::ssh("user@host");
    let _ = Executor::docker_exec("container");
}
