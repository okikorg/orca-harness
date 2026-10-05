//! Host-owned Docker containers: a container the host names and labels
//! with an opaque owner, so it can find, reattach to, finalize and remove
//! that container across its own restarts, optionally attached to a
//! network the host owns.

use std::sync::Arc;

use orca_harness_core::{Capabilities, Sandbox, SandboxError};

use super::{docker, DockerProvisioner, DockerSandbox, AGENT_USER};
use crate::spec::Network;

/// Label carrying the host's opaque owner identity.
const OWNER_LABEL: &str = "orca.environment";

impl DockerProvisioner {
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

    /// The `docker run` arguments an isolated network attachment adds,
    /// validated before anything is created.
    pub(super) fn network_args(&self) -> Result<Vec<String>, SandboxError> {
        let Some((network, trust)) = &self.isolated_network else {
            return Ok(Vec::new());
        };
        if self.spec.network != Network::Enabled || !docker_name(network) || !docker_name(trust) {
            return Err(SandboxError::Provision(
                "invalid host-owned network attachment".into(),
            ));
        }
        Ok(vec![
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
        ])
    }

    /// The `docker run` arguments a named container adds, validated before
    /// anything is created.
    pub(super) fn identity_args(&self) -> Result<Vec<String>, SandboxError> {
        let Some((name, owner)) = &self.identity else {
            return Ok(Vec::new());
        };
        if !docker_name(name) || owner.is_empty() {
            return Err(SandboxError::Provision(
                "invalid managed Docker identity".into(),
            ));
        }
        Ok(vec![
            "--name".into(),
            name.clone(),
            "--label".into(),
            format!("{OWNER_LABEL}={owner}"),
        ])
    }

    /// Remove a managed container only after matching its opaque ownership
    /// label and exact name. Absence is successful, daemon failures are not.
    pub async fn cleanup_named(name: &str, owner: &str) -> Result<(), SandboxError> {
        let output = docker(&[
            "ps".into(),
            "--all".into(),
            "--filter".into(),
            format!("label={OWNER_LABEL}={owner}"),
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
        if container["Config"]["Labels"][OWNER_LABEL] != owner
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
            user: Some(AGENT_USER.into()),
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
    /// unprivileged user. Protected trees and their ancestors remain
    /// root-owned, exactly as for capability directories at provisioning.
    pub async fn finalize_named(
        name: &str,
        owner: &str,
        workspace: &str,
        directories: &[String],
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        if let Some(outside) = directories.iter().find(|directory| {
            *directory != workspace && !directory.starts_with(&format!("{workspace}/"))
        }) {
            return Err(SandboxError::Request(format!(
                "protected directory must be inside workspace: {outside}"
            )));
        }
        let mut sandbox = Self::inspect_named(name, owner, workspace).await?;
        sandbox.user = Some("0:0".into());
        sandbox.protect(directories).await?;
        sandbox.user = Some(AGENT_USER.into());
        Ok(Arc::new(sandbox))
    }
}

/// A Docker object name: nonempty, letters, digits, `_`, `-` and `.` only.
pub(super) fn docker_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::EnvironmentSpec;
    use orca_harness_core::Provisioner;

    #[test]
    fn invalid_identities_are_refused_before_docker_runs() {
        for (name, owner) in [("", "owner"), ("bad name", "owner"), ("ok", "")] {
            let started = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(
                    DockerProvisioner::new(EnvironmentSpec::new())
                        .named(name, owner)
                        .start(),
                );
            match started {
                Err(SandboxError::Provision(message)) => {
                    assert!(message.contains("identity"), "{message}")
                }
                Err(other) => panic!("wrong refusal: {other}"),
                Ok(_) => panic!("an invalid identity was accepted"),
            }
        }
    }

    #[test]
    fn network_attachments_need_an_enabled_network_and_plain_names() {
        for (network, attachment) in [
            (Network::Disabled, ("orca-net", "orca-trust")),
            (Network::Enabled, ("bad net", "orca-trust")),
            (Network::Enabled, ("orca-net", "")),
        ] {
            let provisioner = DockerProvisioner::new(EnvironmentSpec::new().network(network))
                .isolated_network(attachment.0, attachment.1);
            assert!(matches!(
                provisioner.network_args(),
                Err(SandboxError::Provision(_))
            ));
        }
        let args = DockerProvisioner::new(EnvironmentSpec::new().network(Network::Enabled))
            .isolated_network("orca-net", "orca-trust")
            .network_args()
            .unwrap();
        assert_eq!(args[..2], ["--network", "orca-net"]);
        assert!(args.contains(&"NET_RAW".to_string()));
        assert!(args
            .last()
            .unwrap()
            .ends_with("target=/etc/orca-network,readonly"));
    }

    #[test]
    fn finalize_refuses_directories_outside_the_workspace() {
        let refused = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(DockerProvisioner::finalize_named(
                "name",
                "owner",
                "/workspace",
                &["/etc".into()],
            ));
        assert!(matches!(refused, Err(SandboxError::Request(_))));
    }
}
