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

const PROVIDER: &str = "docker";
const DEFAULT_IMAGE: &str = "debian:stable-slim";
/// Enough for the output of one command; the tools truncate above this.
const OUTPUT_CHANNEL_CHUNKS: usize = 32;
/// `stat` exit code for "nothing at this path", distinct from a failure.
const STAT_ABSENT: i32 = 3;

/// Creates local Docker sandboxes.
pub struct DockerProvisioner {
    spec: EnvironmentSpec,
    identity: Option<(String, String)>,
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

    /// Bind a managed container name to a host-owned opaque identity, so the
    /// host can reconnect after restart without provisioning a replacement.
    pub fn named(mut self, name: impl Into<String>, owner: impl Into<String>) -> Self {
        self.identity = Some((name.into(), owner.into()));
        self
    }

    /// Attach to a host-owned isolated network and mount its public trust volume.
    /// The host must enforce and verify network topology and proxy policy; this
    /// mechanism does not advertise restricted-domain enforcement by itself.
    /// Incompatible with Disabled/Restricted specs to avoid overriding them.
    pub fn isolated_network(
        mut self,
        name: impl Into<String>,
        trust_volume: impl Into<String>,
    ) -> Self {
        self.isolated_network = Some((name.into(), trust_volume.into()));
        self
    }

    /// Remove a managed container only after matching its opaque ownership
    /// label and exact name. Absence is successful, daemon failures are not.
    pub async fn cleanup_named(name: &str, owner: &str) -> Result<(), SandboxError> {
        let output = docker(&[
            "ps".into(),
            "--all".into(),
            "--filter".into(),
            format!("label=orca.environment={owner}"),
            "--format".into(),
            "{{json .}}".into(),
        ])
        .await?;
        if !output.status.success() {
            return Err(SandboxError::Request(
                "could not inventory managed Docker containers".into(),
            ));
        }
        for line in String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.is_empty())
        {
            let row: serde_json::Value = serde_json::from_str(line)
                .map_err(|_| SandboxError::Request("invalid Docker inventory".into()))?;
            if row["Names"] != name {
                return Err(SandboxError::Request(
                    "managed Docker container was renamed".into(),
                ));
            }
            let id = row["ID"].as_str().ok_or_else(|| {
                SandboxError::Request("Docker inventory has no container id".into())
            })?;
            let removed = docker(&["rm".into(), "-f".into(), id.into()]).await?;
            if !removed.status.success() {
                return Err(SandboxError::Request(
                    "managed Docker cleanup failed".into(),
                ));
            }
        }
        Ok(())
    }

    /// Reconnect only to a running container bearing the expected identity.
    /// Container names are not sufficient proof of ownership on their own.
    pub async fn attach_named(
        name: &str,
        owner: &str,
        workspace: &str,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        Ok(Arc::new(Self::inspect_named(name, owner, workspace).await?))
    }

    async fn inspect_named(
        name: &str,
        owner: &str,
        workspace: &str,
    ) -> Result<DockerSandbox, SandboxError> {
        let output = docker(&["inspect".into(), name.into()]).await?;
        if !output.status.success() {
            return Err(SandboxError::Request(
                "managed Docker environment is unavailable".into(),
            ));
        }
        let rows: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| SandboxError::Request("invalid Docker inspect response".into()))?;
        let container = rows
            .as_array()
            .and_then(|rows| rows.first())
            .ok_or_else(|| {
                SandboxError::Request("managed Docker environment is unavailable".into())
            })?;
        if container["Config"]["Labels"]["orca.environment"] != owner
            || container["State"]["Running"] != true
            || container["Config"]["WorkingDir"] != workspace
        {
            return Err(SandboxError::Request(
                "Docker environment identity or state does not match".into(),
            ));
        }
        let id = container["Id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| SandboxError::Request("Docker environment has no id".into()))?;
        Ok(DockerSandbox {
            user: Some("65532:65532".into()),
            id: id.into(),
            workspace: workspace.into(),
            capabilities: Capabilities {
                sessions: true,
                file_api: true,
                network_policy: false,
            },
        })
    }

    /// Complete host-owned setup and expose all subsequent operations as an
    /// unprivileged user. Protected trees and their ancestors remain root-owned.
    pub async fn finalize_named(
        name: &str,
        owner: &str,
        workspace: &str,
        directories: &[String],
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        let mut sandbox = Self::inspect_named(name, owner, workspace).await?;
        sandbox.user = Some("0:0".into());
        sandbox
            .run_setup(&format!(
                "chown -hR 65532:65532 {0} && chown 0:0 {0} && chmod 1777 {0}",
                shell_quote(workspace)
            ))
            .await?;
        let mut ancestors = std::collections::BTreeSet::new();
        for directory in directories {
            if directory != workspace && !directory.starts_with(&format!("{workspace}/")) {
                return Err(SandboxError::Request(
                    "protected directory must be inside workspace".into(),
                ));
            }
            let mut path = std::path::Path::new(directory).parent();
            while let Some(parent) = path {
                let text = parent.to_string_lossy();
                if text == workspace {
                    break;
                }
                if !text.starts_with(&format!("{workspace}/")) {
                    break;
                }
                ancestors.insert(text.into_owned());
                path = parent.parent();
            }
        }
        for directory in ancestors {
            sandbox
                .run_setup(&format!(
                    "chown 0:0 {0} && chmod 1777 {0}",
                    shell_quote(&directory)
                ))
                .await?;
        }
        for directory in directories {
            sandbox.run_setup(&format!("test -z \"$(find {0} -type l -print -quit)\" && chown -hR 0:0 {0} && chmod -R a-w {0}", shell_quote(directory))).await?;
        }
        sandbox.user = Some("65532:65532".into());
        Ok(Arc::new(sandbox))
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
            "--init".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "-w".into(),
            self.spec.workspace_directory.clone(),
        ];
        if let Some((name, owner)) = &self.identity {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
                || owner.is_empty()
            {
                return Err(SandboxError::Provision(
                    "invalid managed Docker identity".into(),
                ));
            }
            args.extend([
                "--name".into(),
                name.clone(),
                "--label".into(),
                format!("orca.environment={owner}"),
            ]);
        }
        if matches!(self.spec.network, Network::Disabled) {
            args.push("--network".into());
            args.push("none".into());
        }
        if let Some((network, trust)) = &self.isolated_network {
            if self.spec.network != Network::Enabled
                || [network, trust].iter().any(|name| {
                    name.is_empty()
                        || !name.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                        })
                })
            {
                return Err(SandboxError::Provision(
                    "invalid host-owned network attachment".into(),
                ));
            }
            args.extend([
                "--network".into(),
                network.clone(),
                "--cap-drop".into(),
                "NET_RAW".into(),
                "--cap-drop".into(),
                "NET_ADMIN".into(),
                "--dns".into(),
                "127.0.0.1".into(),
                "--sysctl".into(),
                "net.ipv6.conf.all.disable_ipv6=1".into(),
                "--mount".into(),
                format!("type=volume,source={trust},target=/etc/orca-network,readonly"),
            ]);
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
            user: None,
            id,
            workspace: self.spec.workspace_directory.clone(),
            capabilities: self.capabilities(),
        };
        if let Err(error) = sandbox.prepare(&self.spec).await {
            let _ = sandbox.shutdown().await;
            return Err(error);
        }
        Ok(Arc::new(sandbox))
    }
}

pub struct DockerSandbox {
    user: Option<String>,
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
            let names = spec
                .packages
                .system
                .iter()
                .map(|name| shell_quote(name))
                .collect::<Vec<_>>()
                .join(" ");
            self.run_setup(&format!(
                "apt-get update && apt-get install -y --no-install-recommends {names}"
            ))
            .await?;
        }
        if !spec.packages.python.is_empty() {
            self.run_setup(&format!(
                "pip install {}",
                spec.packages
                    .python
                    .iter()
                    .map(|name| shell_quote(name))
                    .collect::<Vec<_>>()
                    .join(" ")
            ))
            .await?;
        }
        if !spec.packages.npm.is_empty() {
            self.run_setup(&format!(
                "npm install -g {}",
                spec.packages
                    .npm
                    .iter()
                    .map(|name| shell_quote(name))
                    .collect::<Vec<_>>()
                    .join(" ")
            ))
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
        let drain = async { while output.recv().await.is_some() {} };
        let (written, ()) = tokio::join!(write, drain);
        written?;
        if session.wait().await? != Some(0) {
            return Err(SandboxError::Request("Sandbox file write failed".into()));
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
            return Err(SandboxError::Request(
                "Docker sandbox shutdown failed".into(),
            ));
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

/// Single-quote for `sh`, closing and reopening around embedded quotes.
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// `stat -c '%.9Y %s %F'` — modification time as epoch seconds with a
/// nanosecond fraction, size, and a type description whose wording varies
/// ("directory", "regular file", "symbolic link"), so only the directory
/// case is matched by name. Whole seconds (`%Y`, or a `stat` without the
/// precision flag) still parse; they are just coarser.
pub(crate) fn parse_stat(line: &str) -> Option<Stat> {
    let mut parts = line.trim().splitn(3, ' ');
    let modified = parse_epoch(parts.next()?)?;
    let len = parts.next()?.parse::<u64>().ok()?;
    let is_dir = parts.next().is_some_and(|kind| kind.trim() == "directory");
    Some(Stat {
        modified: Some(modified),
        len,
        is_dir,
    })
}

/// `seconds[.fraction]`, keeping up to nine fractional digits exactly
/// rather than going through a float.
fn parse_epoch(text: &str) -> Option<std::time::SystemTime> {
    let (secs, fraction) = text.split_once('.').unwrap_or((text, ""));
    let secs = secs.parse::<u64>().ok()?;
    if fraction.len() > 9 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let nanos = format!("{fraction:0<9}").parse::<u32>().ok()?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::new(secs, nanos))
}

/// `ls -Ap` marks directories with a trailing slash and omits `.`/`..`.
pub(crate) fn parse_ls(listing: &str) -> Vec<Entry> {
    listing
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(|line| match line.strip_suffix('/') {
            Some(name) => Entry {
                name: name.to_string(),
                is_dir: true,
            },
            None => Entry {
                name: line.to_string(),
                is_dir: false,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_embedded_quotes_and_spaces() {
        assert_eq!(shell_quote("/workspace/a b"), "'/workspace/a b'");
        // The classic break: a single quote inside the value must close,
        // escape, and reopen rather than terminate the argument early.
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a;rm -rf /"), "'a;rm -rf /'");
    }

    #[test]
    fn ls_listing_separates_directories_from_files() {
        let entries = parse_ls("src/\nCargo.toml\ntarget/\n\n");
        assert_eq!(
            entries,
            vec![
                Entry {
                    name: "src".into(),
                    is_dir: true
                },
                Entry {
                    name: "Cargo.toml".into(),
                    is_dir: false
                },
                Entry {
                    name: "target".into(),
                    is_dir: true
                },
            ]
        );
    }

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
    fn stat_output_parses_into_a_comparable_stamp() {
        let stat = parse_stat("1757635200.123456789 4096 regular file\n").expect("parse");
        assert_eq!(
            stat.modified,
            Some(std::time::UNIX_EPOCH + std::time::Duration::new(1757635200, 123456789))
        );
        assert_eq!(stat.len, 4096);
        assert!(!stat.is_dir);

        let dir = parse_stat("1757635200 64 directory").expect("whole seconds still parse");
        assert!(dir.is_dir);
        assert_eq!(
            dir.modified,
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1757635200))
        );

        // Garbage must not become a stamp that silently compares equal.
        assert!(parse_stat("").is_none());
        assert!(parse_stat("nonsense").is_none());
        assert!(parse_stat("1757635200.1x 4 regular file").is_none());
        assert!(parse_stat("1757635200.1234567890 4 regular file").is_none());
    }

    #[test]
    fn edits_within_one_second_get_different_stamps() {
        // The guard compares mtime and length; two same-length writes in
        // the same second must still differ.
        let first = parse_stat("1757635200.100000000 5 regular file").unwrap();
        let second = parse_stat("1757635200.500000000 5 regular file").unwrap();
        assert_ne!(first, second);
        assert_eq!(
            parse_stat("1757635200.5 5 regular file").unwrap(),
            second,
            "a short fraction is tenths, not nanoseconds"
        );
    }

    #[test]
    fn sessions_are_advertised_because_docker_exec_accepts_stdin() {
        let caps = DockerProvisioner::new(EnvironmentSpec::new()).capabilities();
        assert!(caps.sessions, "docker exec -i gives a real stdin channel");
        assert!(caps.file_api);
    }
}
