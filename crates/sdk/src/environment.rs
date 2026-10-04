//! Session-specific execution binding. Provisioning and teardown belong to the host.
use crate::{agent::AgentDefinition, background::ProcessConfig, tools::ToolSource};
use crate::{SdkError, ToolPreset};
use orca_harness_core::Sandbox;
use orca_harness_tools::{Executor, Workspace};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// Routing for SDK-built filesystem, command and REPL tools. Caller-owned
/// custom tools, extensions, skills, MCP and memory remain the host's responsibility.
/// A binding ID is a stable host identity, not a credential. Resume verifies
/// that identity and workspace; the host must supply the corresponding live
/// sandbox handle. The SDK never provisions or shuts down a sandbox.
///
/// Persistent sessions require their `.environment.json` companion file,
/// including local sessions. Missing/corrupt bindings fail closed; transcripts
/// created before this API require explicit host migration.
#[derive(Clone, Default)]
pub enum SessionEnvironment {
    /// Compatibility mode: use the agent's local workspace and process recipe.
    #[default]
    Local,
    /// Omit filesystem, command and REPL built-ins, including in children.
    Disabled,
    /// A host-provisioned sandbox; construct with [`Self::sandbox`].
    Sandbox {
        binding_id: String,
        sandbox: Arc<dyn Sandbox>,
        workspace: PathBuf,
    },
}

impl SessionEnvironment {
    pub fn sandbox(
        binding_id: impl Into<String>,
        sandbox: Arc<dyn Sandbox>,
        workspace: impl Into<PathBuf>,
    ) -> Result<Self, SdkError> {
        let value = Self::Sandbox {
            binding_id: binding_id.into(),
            sandbox,
            workspace: workspace.into(),
        };
        value.validate_shape()?;
        Ok(value)
    }

    fn validate_shape(&self) -> Result<(), SdkError> {
        if let Self::Sandbox {
            binding_id,
            workspace,
            ..
        } = self
        {
            if binding_id.trim().is_empty()
                || !workspace.is_absolute()
                || workspace
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(SdkError::Config("sandbox requires a nonempty binding id and an absolute workspace without parent traversal".into()));
            }
        }
        Ok(())
    }

    pub(crate) fn validate(&self, definition: &AgentDefinition) -> Result<(), SdkError> {
        self.validate_shape()?;
        if let Self::Sandbox { sandbox, .. } = self {
            let capabilities = sandbox.capabilities();
            if (definition.preset != ToolPreset::None
                || definition
                    .tool_sources
                    .iter()
                    .any(|s| matches!(s, ToolSource::Bun)))
                && !capabilities.file_api
            {
                return Err(SdkError::Config(
                    "sandbox lacks file_api required by tool preset or Bun REPL".into(),
                ));
            }
            if (definition.preset == ToolPreset::Coding
                || definition
                    .tool_sources
                    .iter()
                    .any(|s| matches!(s, ToolSource::Python | ToolSource::Bun)))
                && !capabilities.sessions
            {
                return Err(SdkError::Config(
                    "sandbox lacks sessions required by process/REPL tools".into(),
                ));
            }
            if definition
                .processes
                .as_ref()
                .is_some_and(|p| p.executor.is_some() || p.working_dir.is_some())
            {
                return Err(SdkError::Config("sandbox environment conflicts with explicit process executor or working directory".into()));
            }
        }
        Ok(())
    }

    pub(crate) fn workspace(&self, definition: &AgentDefinition) -> Workspace {
        match self {
            Self::Sandbox {
                sandbox, workspace, ..
            } => Workspace::sandboxed(workspace.clone(), sandbox.clone()),
            _ => definition.harness.workspace().clone(),
        }
    }

    pub(crate) fn processes(&self, definition: &AgentDefinition) -> Option<ProcessConfig> {
        match self {
            Self::Sandbox {
                sandbox, workspace, ..
            } => Some(
                definition
                    .processes
                    .clone()
                    .unwrap_or_default()
                    .executor(Executor::sandbox(sandbox.clone()))
                    .working_dir(workspace.clone()),
            ),
            _ => definition.processes.clone(),
        }
    }

    pub(crate) fn preset(&self, preset: ToolPreset) -> ToolPreset {
        if matches!(self, Self::Disabled) {
            ToolPreset::None
        } else {
            preset
        }
    }

    fn binding(&self) -> serde_json::Value {
        match self {
            Self::Local => serde_json::json!({"version":1,"mode":"local"}),
            Self::Disabled => serde_json::json!({"version":1,"mode":"disabled"}),
            Self::Sandbox {
                binding_id,
                workspace,
                ..
            } => {
                serde_json::json!({"version":1,"mode":"sandbox","binding_id":binding_id,"workspace":workspace})
            }
        }
    }

    pub(crate) fn persist(&self, path: &Path) -> Result<(), SdkError> {
        std::fs::write(
            path.with_extension("environment.json"),
            self.binding().to_string(),
        )?;
        Ok(())
    }

    pub(crate) fn verify(&self, path: &Path) -> Result<(), SdkError> {
        let bytes = match std::fs::read(path.with_extension("environment.json")) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SdkError::Config(
                    "session has no persisted environment binding".into(),
                ))
            }
            Err(e) => return Err(e.into()),
        };
        let binding: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| SdkError::Config(format!("invalid session environment binding: {e}")))?;
        if binding != self.binding() {
            return Err(SdkError::Config(
                "session environment binding mismatch; supply the original environment".into(),
            ));
        }
        Ok(())
    }
}
