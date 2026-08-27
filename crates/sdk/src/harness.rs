use std::path::{Path, PathBuf};
use std::sync::Arc;

use orca_harness_core::Model;
use orca_harness_extensions::{workspace_key, MemoryScope, MemoryStore};
use orca_harness_tools::Workspace;

use crate::{AgentBuilder, Mcp, Memory, SdkError, Sessions, Skills};

#[derive(Clone)]
pub struct Harness {
    pub(crate) inner: Arc<HarnessInner>,
}

pub(crate) struct HarnessInner {
    pub workspace: Workspace,
    pub state_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub workspace_id: String,
}

#[derive(Default)]
pub struct HarnessBuilder {
    workspace: Option<PathBuf>,
    state_dir: Option<PathBuf>,
}

impl Harness {
    pub fn builder() -> HarnessBuilder {
        HarnessBuilder::default()
    }

    pub fn workspace(&self) -> &Workspace {
        &self.inner.workspace
    }

    pub fn state_dir(&self) -> &Path {
        &self.inner.state_dir
    }

    pub fn agent(&self, model: impl Model + 'static) -> AgentBuilder {
        AgentBuilder::new(self.clone(), Arc::new(model))
    }

    pub fn sessions(&self) -> Sessions {
        Sessions::new(self.inner.sessions_dir.clone())
    }

    pub fn memory(&self) -> Result<Memory, SdkError> {
        let store = MemoryStore::open(self.inner.state_dir.join("memory.sqlite3"))?;
        let scope = MemoryScope::new(
            self.inner.workspace_id.clone(),
            self.inner.workspace.root().display().to_string(),
        );
        Ok(Memory::new(store, scope))
    }

    pub fn mcp(&self) -> Mcp {
        Mcp::new()
    }

    pub fn skills(&self) -> Skills {
        Skills::new(
            self.inner.workspace.root(),
            Some(self.inner.state_dir.clone()),
            std::env::var_os("HOME").map(PathBuf::from),
        )
    }
}

impl HarnessBuilder {
    pub fn workspace(mut self, path: impl Into<PathBuf>) -> Self {
        self.workspace = Some(path.into());
        self
    }

    pub fn state_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(path.into());
        self
    }

    pub fn build(self) -> Result<Harness, SdkError> {
        let workspace = match self.workspace {
            Some(path) => path,
            None => std::env::current_dir()?,
        };
        let workspace = workspace.canonicalize()?;
        if !workspace.is_dir() {
            return Err(SdkError::Config(format!(
                "workspace is not a directory: {}",
                workspace.display()
            )));
        }
        let state_dir = self.state_dir.unwrap_or_else(|| workspace.join(".orca"));
        std::fs::create_dir_all(&state_dir)?;
        let state_dir = state_dir.canonicalize()?;
        let workspace_text = workspace.display().to_string();
        let workspace_id = workspace_key(&workspace_text);
        let sessions_dir = state_dir.join("sessions").join(&workspace_id);
        Ok(Harness {
            inner: Arc::new(HarnessInner {
                workspace: Workspace::new(workspace),
                state_dir,
                sessions_dir,
                workspace_id,
            }),
        })
    }
}
