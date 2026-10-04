//! Local Docker sandbox: the keyless adapter.
//!
//! The analogue of `Provider::Local` on the model side — no account, no API
//! key, so the whole sandbox path is exercisable in tests and on a laptop.
//! It drives the `docker` CLI rather than linking a Docker client library,
//! which keeps the dependency budget at zero and reuses the same
//! process-spawning the harness already does everywhere else.
//!
//! It also supports [`spawn`](orca_harness_core::Sandbox::spawn) honestly:
//! `docker exec -i` gives a real stdin channel to a live process, so this
//! adapter reports `sessions: true` and can host the tools that need one.

use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use orca_harness_core::{
    CancellationToken, Capabilities, Chunk, Entry, ExecOutput, ExecRequest, FileMode, Output,
    Provisioner, Sandbox, SandboxError, Session, SpawnRequest, Stat,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::spec::{EnvironmentSpec, Network};

mod parse;

use parse::{parse_ls, parse_stat, shell_quote};

const PROVIDER: &str = "docker";
const DEFAULT_IMAGE: &str = "debian:stable-slim";
/// Enough for the output of one command; the tools truncate above this.
const OUTPUT_CHANNEL_CHUNKS: usize = 32;

/// Creates local Docker sandboxes.
pub struct DockerProvisioner {
    spec: EnvironmentSpec,
}

impl DockerProvisioner {
    pub fn new(spec: EnvironmentSpec) -> Self {
        Self { spec }
    }
}

#[async_trait]
impl Provisioner for DockerProvisioner {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            sessions: true,
            file_api: true,
            // `docker run --network none` is all-or-nothing; per-domain
            // rules would need a proxy, so this adapter does not claim
            // them and a host requiring them refuses to start here.
            network_policy: false,
        }
    }

    async fn start(&self) -> Result<Arc<dyn Sandbox>, SandboxError> {
        if matches!(self.spec.network, Network::Restricted { .. }) {
            return Err(SandboxError::Unsupported {
                provider: PROVIDER,
                capability: "restricted network policy",
            });
        }

        let image = self.spec.image.as_deref().unwrap_or(DEFAULT_IMAGE);
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--rm".into(),
            "-w".into(),
            self.spec.workspace_directory.clone(),
        ];
        if matches!(self.spec.network, Network::Disabled) {
            args.push("--network".into());
            args.push("none".into());
        }
        for (key, value) in &self.spec.env {
            args.push("-e".into());
            args.push(format!("{key}={value}"));
        }
        args.push(image.into());
        // The container must outlive the command that created it; the
        // agent's work arrives later through `exec` and `spawn`.
        args.extend(["sleep".to_string(), "infinity".to_string()]);

        let output = docker(&args).await?;
        let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if id.is_empty() {
            return Err(SandboxError::Provision(format!(
                "docker run produced no container id: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        let sandbox = DockerSandbox {
            id,
            workspace: self.spec.workspace_directory.clone(),
            capabilities: self.capabilities(),
        };
        sandbox.prepare(&self.spec).await?;
        Ok(Arc::new(sandbox))
    }
}

pub struct DockerSandbox {
    id: String,
    workspace: String,
    capabilities: Capabilities,
}

impl DockerSandbox {
    /// Create the workspace, install packages, run setup commands, and
    /// make the capability directories read-only. Any failure here fails
    /// the whole provisioning: a half-prepared sandbox is worse than none.
    async fn prepare(&self, spec: &EnvironmentSpec) -> Result<(), SandboxError> {
        self.run_setup(&format!("mkdir -p {}", shell_quote(&self.workspace)))
            .await?;

        if !spec.packages.system.is_empty() {
            let names = spec.packages.system.join(" ");
            self.run_setup(&format!(
                "apt-get update && apt-get install -y --no-install-recommends {names}"
            ))
            .await?;
        }
        if !spec.packages.python.is_empty() {
            self.run_setup(&format!("pip install {}", spec.packages.python.join(" ")))
                .await?;
        }
        if !spec.packages.npm.is_empty() {
            self.run_setup(&format!("npm install -g {}", spec.packages.npm.join(" ")))
                .await?;
        }

        for setup in &spec.setup_commands {
            let mut request = ExecRequest::new(&setup.command);
            request.cwd = setup.cwd.clone().or_else(|| Some(self.workspace.clone()));
            let output = self.exec(request).await?;
            if output.exit_code != 0 {
                return Err(SandboxError::Provision(format!(
                    "setup command failed ({}): {}",
                    setup.command,
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }

        for dir in &spec.capability_directories {
            self.run_setup(&format!(
                "mkdir -p {dir} && chmod -R a-w {dir}",
                dir = shell_quote(dir)
            ))
            .await?;
        }
        Ok(())
    }

    async fn run_setup(&self, command: &str) -> Result<(), SandboxError> {
        let output = self.exec(ExecRequest::new(command)).await?;
        if output.exit_code != 0 {
            return Err(SandboxError::Provision(format!(
                "{command}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    fn exec_args(&self, cwd: Option<&str>, interactive: bool) -> Vec<String> {
        let mut args = vec!["exec".to_string()];
        if interactive {
            args.push("-i".into());
        }
        args.push("-w".into());
        args.push(cwd.unwrap_or(&self.workspace).to_string());
        args.push(self.id.clone());
        args
    }
}

#[async_trait]
impl Sandbox for DockerSandbox {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    async fn exec(&self, request: ExecRequest) -> Result<ExecOutput, SandboxError> {
        let mut args = self.exec_args(request.cwd.as_deref(), false);
        for (key, value) in &request.env {
            args.insert(1, format!("{key}={value}"));
            args.insert(1, "-e".into());
        }
        args.push("sh".into());
        args.push("-c".into());
        args.push(request.command.clone());

        let run = docker(&args);
        let output = match request.timeout_ms {
            Some(ms) => tokio::time::timeout(std::time::Duration::from_millis(ms), run)
                .await
                .map_err(|_| SandboxError::Timeout(request.command.clone()))??,
            None => run.await?,
        };
        Ok(ExecOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    async fn spawn(
        &self,
        request: SpawnRequest,
    ) -> Result<(Arc<dyn Session>, Output), SandboxError> {
        let mut args = self.exec_args(request.cwd.as_deref(), true);
        for (key, value) in &request.env {
            args.insert(1, format!("{key}={value}"));
            args.insert(1, "-e".into());
        }
        args.push(request.program.clone());
        args.extend(request.args.iter().cloned());

        let mut child = Command::new("docker")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| SandboxError::Request(format!("docker exec -i failed to start: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SandboxError::Request("docker exec gave no stdin".into()))?;
        let (tx, rx) = tokio::sync::mpsc::channel(OUTPUT_CHANNEL_CHUNKS);
        if let Some(pipe) = child.stdout.take() {
            pump(pipe, tx.clone(), false);
        }
        if let Some(pipe) = child.stderr.take() {
            pump(pipe, tx, true);
        }

        // The child is owned by a supervisor task rather than a mutex:
        // `wait` would hold that mutex for the whole life of the process
        // and deadlock the `kill` that is supposed to end it.
        let (exit_tx, exit_rx) = tokio::sync::watch::channel(None);
        let kill = CancellationToken::new();
        let killed = kill.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                biased;
                _ = killed.cancelled() => {
                    let _ = child.start_kill();
                    child.wait().await
                }
                status = child.wait() => status,
            };
            let _ = exit_tx.send(Some(status.ok().and_then(|s| s.code())));
        });

        let session = DockerSession {
            stdin: Mutex::new(Some(stdin)),
            exit: exit_rx,
            kill,
        };
        Ok((Arc::new(session), rx))
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError> {
        let mut args = self.exec_args(None, false);
        args.extend(["cat".to_string(), path.to_string()]);
        let output = docker(&args).await?;
        if !output.status.success() {
            return Err(SandboxError::Request(format!(
                "read {path}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout)
    }

    async fn write_file(
        &self,
        path: &str,
        bytes: &[u8],
        mode: FileMode,
    ) -> Result<(), SandboxError> {
        let quoted = shell_quote(path);
        let mut args = self.exec_args(None, true);
        args.push("sh".into());
        args.push("-c".into());
        // Written through stdin rather than an argument so that binary
        // content and arbitrary bytes survive intact.
        args.push(format!(
            "mkdir -p \"$(dirname {quoted})\" && cat > {quoted}"
        ));

        let mut child = Command::new("docker")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| SandboxError::Request(format!("docker exec failed to start: {e}")))?;
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| SandboxError::Request("docker exec gave no stdin".into()))?;
            stdin
                .write_all(bytes)
                .await
                .map_err(|e| SandboxError::Request(format!("write {path}: {e}")))?;
            stdin
                .shutdown()
                .await
                .map_err(|e| SandboxError::Request(format!("write {path}: {e}")))?;
        }
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| SandboxError::Request(format!("write {path}: {e}")))?;
        if !output.status.success() {
            return Err(SandboxError::Request(format!(
                "write {path}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        if mode == FileMode::Executable {
            self.run_setup(&format!("chmod +x {quoted}")).await?;
        }
        Ok(())
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<Entry>, SandboxError> {
        let mut args = self.exec_args(None, false);
        args.extend([
            "sh".to_string(),
            "-c".to_string(),
            format!("ls -Ap {}", shell_quote(path)),
        ]);
        let output = docker(&args).await?;
        if !output.status.success() {
            return Err(SandboxError::Request(format!(
                "list {path}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(parse_ls(&String::from_utf8_lossy(&output.stdout)))
    }

    async fn stat(&self, path: &str) -> Result<Option<Stat>, SandboxError> {
        let mut args = self.exec_args(None, false);
        // `%Y` mtime, `%s` size, `%F` human-readable type. One call, and
        // a missing file is reported as absence rather than failure.
        args.extend([
            "stat".to_string(),
            "-c".to_string(),
            "%Y %s %F".to_string(),
            path.to_string(),
        ]);
        let output = docker(&args).await?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(parse_stat(&String::from_utf8_lossy(&output.stdout)))
    }

    async fn shutdown(&self) -> Result<(), SandboxError> {
        // `--rm` removes it once stopped; force so a busy container still
        // goes away rather than leaking past the session.
        docker(&["rm".into(), "-f".into(), self.id.clone()]).await?;
        Ok(())
    }
}

struct DockerSession {
    /// `None` once stdin has been closed; the pipe is dropped to make the
    /// process see EOF.
    stdin: Mutex<Option<tokio::process::ChildStdin>>,
    /// `Some(code)` once the supervisor has reaped the child.
    exit: tokio::sync::watch::Receiver<Option<Option<i32>>>,
    kill: CancellationToken,
}

#[async_trait]
impl Session for DockerSession {
    async fn write_stdin(&self, bytes: &[u8]) -> Result<(), SandboxError> {
        let mut guard = self.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| SandboxError::Request("stdin is closed".into()))?;
        stdin
            .write_all(bytes)
            .await
            .map_err(|e| SandboxError::Request(format!("stdin write failed: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| SandboxError::Request(format!("stdin flush failed: {e}")))
    }

    async fn close_stdin(&self) -> Result<(), SandboxError> {
        *self.stdin.lock().await = None;
        Ok(())
    }

    async fn wait(&self) -> Result<Option<i32>, SandboxError> {
        let mut exit = self.exit.clone();
        // `borrow` first: the supervisor may have finished before this
        // receiver was cloned, and `changed` only reports what comes next.
        if let Some(code) = *exit.borrow_and_update() {
            return Ok(code);
        }
        while exit.changed().await.is_ok() {
            if let Some(code) = *exit.borrow_and_update() {
                return Ok(code);
            }
        }
        // The supervisor task is gone without reporting — the runtime is
        // shutting down. Staying pending would be a lie of a different
        // kind, but claiming an exit we never saw is the worse one.
        Err(SandboxError::Terminated("supervisor stopped".into()))
    }

    async fn kill(&self) -> Result<(), SandboxError> {
        self.kill.cancel();
        Ok(())
    }
}

/// Backpressure the reader instead of letting a runaway program queue
/// output without limit — the same shape the REPL tools already use.
fn pump<R>(mut pipe: R, tx: tokio::sync::mpsc::Sender<Chunk>, stderr: bool)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx
                        .send(Chunk {
                            stderr,
                            bytes: buf[..n].to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
}

async fn docker(args: &[String]) -> Result<std::process::Output, SandboxError> {
    Command::new("docker")
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                SandboxError::Provision("the `docker` CLI is not on PATH".into())
            }
            _ => SandboxError::Request(format!(
                "docker {}: {e}",
                args.first().cloned().unwrap_or_default()
            )),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricted_network_is_refused_rather_than_ignored() {
        // The adapter cannot enforce per-domain egress, and says so in its
        // capabilities. Accepting the spec anyway would run wide open
        // while the host believed it was restricted.
        let provisioner =
            DockerProvisioner::new(EnvironmentSpec::new().network(Network::Restricted {
                allowed_domains: vec!["api.github.com".into()],
            }));
        assert!(!provisioner.capabilities().network_policy);

        let started = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(provisioner.start());
        match started {
            Err(SandboxError::Unsupported {
                provider: "docker", ..
            }) => {}
            Err(other) => panic!("wrong refusal: {other}"),
            Ok(_) => panic!("restricted network must be refused, not silently ignored"),
        }
    }

    #[test]
    fn sessions_are_advertised_because_docker_exec_accepts_stdin() {
        let caps = DockerProvisioner::new(EnvironmentSpec::new()).capabilities();
        assert!(caps.sessions, "docker exec -i gives a real stdin channel");
        assert!(caps.file_api);
    }
}
