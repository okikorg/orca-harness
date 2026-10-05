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

mod named;
mod parse;
mod process;

use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use orca_harness_core::{
    CancellationToken, Capabilities, Chunk, Entry, ExecOutput, ExecRequest, FileMode, Output,
    Provisioner, Sandbox, SandboxError, Session, SpawnRequest, Stat,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::spec::{EnvironmentSpec, Network};

use parse::{parse_ls, parse_stat, shell_quote};

const PROVIDER: &str = "docker";
const DEFAULT_IMAGE: &str = "debian:stable-slim";
/// Enough for the output of one command; the tools truncate above this.
const OUTPUT_CHANNEL_CHUNKS: usize = 32;
/// Who agent operations run as once capability directories are protected.
/// Root can write through any permission bit, so read-only only means
/// something to an identity that does not own the protected tree.
const AGENT_USER: &str = "65532:65532";
/// `stat` exit code for "nothing at this path", distinct from a failure.
const STAT_ABSENT: i32 = 3;
/// Label carrying a container's provisioning token, so one whose
/// `docker run` was interrupted can still be found and removed.
const SANDBOX_LABEL: &str = "orca.sandbox";
static NEXT_SANDBOX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Creates local Docker sandboxes.
pub struct DockerProvisioner {
    spec: EnvironmentSpec,
    /// Container name and opaque owner label; see [`Self::named`].
    identity: Option<(String, String)>,
    /// Host network and trust volume; see [`Self::isolated_network`].
    isolated_network: Option<(String, String)>,
}

impl DockerProvisioner {
    pub fn new(spec: EnvironmentSpec) -> Self {
        Self {
            spec,
            identity: None,
            isolated_network: None,
        }
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

        // A name like `--index-url=...` would reach the installer as an
        // option; quoting stops the shell, not the installer's own parser.
        let packages = &self.spec.packages;
        if let Some(name) = packages
            .system
            .iter()
            .chain(&packages.python)
            .chain(&packages.npm)
            .find(|name| name.is_empty() || name.starts_with('-'))
        {
            return Err(SandboxError::Provision(format!(
                "invalid package name {name:?}: must be non-empty and not start with '-'"
            )));
        }

        let identity = self.identity_args()?;
        let attachment = self.network_args()?;

        // Claimed before `docker run`: if this future is dropped while the
        // daemon is still creating the container, the guard can only find
        // it by this label.
        let token = format!(
            "{:x}-{:x}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            NEXT_SANDBOX.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let mut guard = Unclaimed(Some(token.clone()));

        let image = self.spec.image.as_deref().unwrap_or(DEFAULT_IMAGE);
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--rm".into(),
            "--label".into(),
            format!("{SANDBOX_LABEL}={token}"),
            // Reaps the descendants a killed process group leaves behind.
            "--init".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "-w".into(),
            self.spec.workspace_directory.clone(),
        ];
        args.extend(identity);
        if matches!(self.spec.network, Network::Disabled) {
            args.push("--network".into());
            args.push("none".into());
        }
        args.extend(attachment);
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

        // The container exists and is ours to remove on a failed
        // preparation; the guard still covers cancellation, where no caller
        // will ever hold a handle to shut it down.
        let mut sandbox = DockerSandbox {
            user: None,
            id,
            workspace: self.spec.workspace_directory.clone(),
            capabilities: self.capabilities(),
        };
        let prepared = async {
            sandbox.prepare(&self.spec).await?;
            if !self.spec.capability_directories.is_empty() {
                sandbox.protect(&self.spec.capability_directories).await?;
                sandbox.user = Some(AGENT_USER.into());
            }
            Ok::<_, SandboxError>(())
        }
        .await;
        if let Err(error) = prepared {
            let _ = sandbox.shutdown().await;
            guard.0 = None;
            return Err(error);
        }
        guard.0 = None;
        Ok(Arc::new(sandbox))
    }
}

/// A container being provisioned that no caller owns yet, by its label
/// token. Dropped while still holding it — the provisioning future was
/// cancelled — it removes whatever carries the label.
struct Unclaimed(Option<String>);

impl Drop for Unclaimed {
    fn drop(&mut self) {
        let Some(token) = self.0.take() else {
            return;
        };
        // A `docker run` cut off mid-flight may still create the container
        // after this point, so the sweep looks for up to ten seconds rather
        // than once. It runs as its own process so it outlives this frame,
        // and the runtime if that is shutting down too.
        let sweep = format!(
            "i=0; while [ $i -lt 40 ]; do \
               ids=$(docker ps -aq --filter label={SANDBOX_LABEL}={token}); \
               [ -n \"$ids\" ] && docker rm -f $ids >/dev/null 2>&1 && exit 0; \
               i=$((i + 1)); sleep 0.25; \
             done"
        );
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                if let Ok(mut child) = Command::new("sh")
                    .args(["-c", &sweep])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    runtime.spawn(async move {
                        let _ = child.wait().await;
                    });
                }
            }
            Err(_) => {
                if let Ok(mut child) = std::process::Command::new("sh")
                    .args(["-c", &sweep])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
            }
        }
    }
}

pub struct DockerSandbox {
    /// `None` runs as the image's user (root for the supplied images);
    /// set once capability directories have been protected.
    user: Option<String>,
    id: String,
    workspace: String,
    capabilities: Capabilities,
}

impl DockerSandbox {
    /// Create the workspace, install packages, and run setup commands, all
    /// as root. Any failure here fails the whole provisioning: a
    /// half-prepared sandbox is worse than none.
    async fn prepare(&self, spec: &EnvironmentSpec) -> Result<(), SandboxError> {
        self.run_setup(&format!("mkdir -p {}", shell_quote(&self.workspace)))
            .await?;

        let quoted = |names: &[String]| {
            names
                .iter()
                .map(|name| shell_quote(name))
                .collect::<Vec<_>>()
                .join(" ")
        };
        if !spec.packages.system.is_empty() {
            self.run_setup(&format!(
                "apt-get update && apt-get install -y --no-install-recommends {}",
                quoted(&spec.packages.system)
            ))
            .await?;
        }
        if !spec.packages.python.is_empty() {
            self.run_setup(&format!("pip install {}", quoted(&spec.packages.python)))
                .await?;
        }
        if !spec.packages.npm.is_empty() {
            self.run_setup(&format!("npm install -g {}", quoted(&spec.packages.npm)))
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
        Ok(())
    }

    /// Make capability directories unchangeable by the agent. Runs as root,
    /// before the sandbox switches to [`AGENT_USER`]:
    ///
    /// - the workspace is handed to the agent, so it stays writable;
    /// - each capability tree becomes root-owned and read-only, and is
    ///   refused if it contains a symlink that could point the chown
    ///   elsewhere;
    /// - the workspace and every directory between it and a capability tree
    ///   become root-owned and sticky, so the agent can still create files
    ///   there but cannot rename or delete the protected tree to swap it;
    ///   an ancestor that is a symlink is refused, since the chown and chmod
    ///   would follow it out of the workspace.
    async fn protect(&self, directories: &[String]) -> Result<(), SandboxError> {
        let workspace = &self.workspace;
        let inside = format!("{workspace}/");
        self.run_setup(&format!(
            "chown -hR {AGENT_USER} {0} && chown 0:0 {0} && chmod 1777 {0}",
            shell_quote(workspace)
        ))
        .await?;
        let mut ancestors = std::collections::BTreeSet::new();
        for directory in directories {
            if !directory.starts_with('/') {
                return Err(SandboxError::Provision(format!(
                    "capability directory must be absolute: {directory}"
                )));
            }
            let mut path = std::path::Path::new(directory).parent();
            while let Some(parent) = path {
                let text = parent.to_string_lossy();
                if !text.starts_with(&inside) {
                    break;
                }
                ancestors.insert(text.into_owned());
                path = parent.parent();
            }
        }
        for directory in &ancestors {
            self.run_setup(&format!(
                "test ! -L {0} && mkdir -p {0} && chown 0:0 {0} && chmod 1777 {0}",
                shell_quote(directory)
            ))
            .await?;
        }
        for directory in directories {
            self.run_setup(&format!(
                "mkdir -p {0} && test -z \"$(find {0} -type l -print -quit)\" \
                 && chown -hR 0:0 {0} && chmod -R a-w {0}",
                shell_quote(directory)
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
        if let Some(user) = &self.user {
            args.extend(["--user".into(), user.clone()]);
        }
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
        self.execute_bounded(request).await
    }

    async fn spawn(
        &self,
        request: SpawnRequest,
    ) -> Result<(Arc<dyn Session>, Output), SandboxError> {
        self.start_process(request).await
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError> {
        let output = self
            .execute_bounded(ExecRequest::new(format!("cat -- {}", shell_quote(path))))
            .await?;
        if output.exit_code != 0 {
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
        // Written through stdin rather than an argument so that binary
        // content and arbitrary bytes survive intact.
        let (session, mut output) = self
            .start_process(SpawnRequest {
                program: "sh".into(),
                args: vec![
                    "-c".into(),
                    format!("mkdir -p \"$(dirname {quoted})\" && cat > {quoted}"),
                ],
                ..Default::default()
            })
            .await?;
        let write = async {
            let result = session.write_stdin(bytes).await;
            session.close_stdin().await?;
            result
        };
        let mut stderr = Vec::new();
        let drain = async {
            while let Some(chunk) = output.recv().await {
                if chunk.stderr {
                    stderr.extend(chunk.bytes);
                }
            }
        };
        let (written, ()) = tokio::join!(write, drain);
        written?;
        if session.wait().await? != Some(0) {
            return Err(SandboxError::Request(format!(
                "write {path}: {}",
                String::from_utf8_lossy(&stderr).trim()
            )));
        }

        if mode == FileMode::Executable {
            self.run_setup(&format!("chmod +x {quoted}")).await?;
        }
        Ok(())
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<Entry>, SandboxError> {
        let output = self
            .execute_bounded(ExecRequest::new(format!("ls -Ap -- {}", shell_quote(path))))
            .await?;
        if output.exit_code != 0 {
            return Err(SandboxError::Request(format!(
                "list {path}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(parse_ls(&String::from_utf8_lossy(&output.stdout)))
    }

    /// Absence is `Ok(None)`; anything else that stops a stamp being read
    /// is an error. Reporting a failed stat as absence would let the
    /// read-before-write guard wave through an overwrite it never checked.
    async fn stat(&self, path: &str) -> Result<Option<Stat>, SandboxError> {
        let quoted = shell_quote(path);
        // `%.9Y` keeps the nanoseconds: whole seconds would let a
        // same-length edit within one second compare equal.
        let output = self
            .execute_bounded(ExecRequest::new(format!(
                "[ -e {quoted} ] || [ -L {quoted} ] || exit {STAT_ABSENT}; \
                 exec stat -c '%.9Y %s %F' -- {quoted}"
            )))
            .await?;
        match output.exit_code {
            0 => parse_stat(&String::from_utf8_lossy(&output.stdout))
                .map(Some)
                .ok_or_else(|| {
                    SandboxError::Request(format!(
                        "stat {path}: unreadable output {:?}",
                        String::from_utf8_lossy(&output.stdout).trim()
                    ))
                }),
            STAT_ABSENT => Ok(None),
            _ => Err(SandboxError::Request(format!(
                "stat {path}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
        }
    }

    async fn shutdown(&self) -> Result<(), SandboxError> {
        // `--rm` removes it once stopped; force so a busy container still
        // goes away rather than leaking past the session.
        let output = docker(&["rm".into(), "-f".into(), self.id.clone()]).await?;
        if !output.status.success() {
            return Err(SandboxError::Request(format!(
                "docker rm {}: {}",
                self.id,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }
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
    use crate::spec::Packages;

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
    fn package_names_that_look_like_options_are_refused() {
        // Refused before any container is created, so this needs no Docker.
        for packages in [
            Packages {
                python: vec!["--index-url=https://example.com/simple".into()],
                ..Default::default()
            },
            Packages::system(["-o", "APT::Get::AllowUnauthenticated=true"]),
            Packages {
                npm: vec![String::new()],
                ..Default::default()
            },
        ] {
            let started = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(
                    DockerProvisioner::new(EnvironmentSpec::new().packages(packages)).start(),
                );
            match started {
                Err(SandboxError::Provision(message)) => {
                    assert!(message.contains("invalid package name"), "{message}")
                }
                Err(other) => panic!("wrong refusal: {other}"),
                Ok(_) => panic!("an option-like package name was accepted"),
            }
        }
    }

    #[test]
    fn sessions_are_advertised_because_docker_exec_accepts_stdin() {
        let caps = DockerProvisioner::new(EnvironmentSpec::new()).capabilities();
        assert!(caps.sessions, "docker exec -i gives a real stdin channel");
        assert!(caps.file_api);
    }
}
