use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use orca_harness_core::Tool;
use orca_harness_tool_extensions::skills::{
    checkout, discover, find_candidates, install, parse_request, scaffold, uninstall, Discovered,
    Installed, Skill, SkillRoot, SkillTool,
};

use crate::SdkError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDestination {
    Managed,
    Workspace,
}

#[derive(Clone)]
pub struct Skills {
    roots: Arc<Vec<SkillRoot>>,
    managed_root: PathBuf,
    workspace_root: PathBuf,
    disabled: Arc<RwLock<HashSet<String>>>,
    discovered: Arc<RwLock<Discovered>>,
}

impl Skills {
    pub fn new(workspace: &Path, config_dir: Option<PathBuf>, home: Option<PathBuf>) -> Self {
        let managed_root = config_dir
            .clone()
            .unwrap_or_else(|| workspace.join(".orca"))
            .join("skills");
        Self {
            roots: Arc::new(orca_harness_tool_extensions::skills::roots(
                workspace,
                config_dir.as_deref(),
                home.as_deref(),
            )),
            managed_root,
            workspace_root: workspace.join(".orca/skills"),
            disabled: Arc::default(),
            discovered: Arc::default(),
        }
    }

    pub fn from_roots(
        roots: Vec<SkillRoot>,
        managed_root: PathBuf,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            roots: Arc::new(roots),
            managed_root,
            workspace_root,
            disabled: Arc::default(),
            discovered: Arc::default(),
        }
    }

    pub fn reload(&self) -> Discovered {
        let found = discover(&self.roots);
        *self.discovered.write().expect("skills lock") = found.clone();
        found
    }

    pub fn catalog(&self) -> Discovered {
        self.discovered.read().expect("skills lock").clone()
    }

    pub fn enable(&self, name: &str) {
        self.disabled.write().expect("skills lock").remove(name);
    }

    pub fn disable(&self, name: &str) {
        self.disabled
            .write()
            .expect("skills lock")
            .insert(name.to_string());
    }

    pub fn tool(&self) -> Option<Arc<dyn Tool>> {
        let disabled = self.disabled.read().expect("skills lock");
        let skills: Vec<Skill> = self
            .discovered
            .read()
            .expect("skills lock")
            .skills
            .iter()
            .filter(|skill| !disabled.contains(&skill.name))
            .cloned()
            .collect();
        (!skills.is_empty()).then(|| Arc::new(SkillTool::new(skills)) as Arc<dyn Tool>)
    }

    pub fn scaffold(&self, name: &str, destination: SkillDestination) -> Result<PathBuf, SdkError> {
        scaffold(self.destination(destination), name).map_err(SdkError::Skill)
    }

    pub async fn install(
        &self,
        source: &str,
        destination: SkillDestination,
    ) -> Result<Vec<Installed>, SdkError> {
        let request = parse_request(source).map_err(SdkError::Skill)?;
        let checkout = checkout(&request.origin).await.map_err(SdkError::Skill)?;
        let mut candidates = find_candidates(&checkout.root);
        if let Some(filter) = request.filter {
            candidates.retain(|candidate| candidate.name == filter);
        }
        if request.list_only {
            return Ok(Vec::new());
        }
        if candidates.is_empty() {
            return Err(SdkError::Skill("no matching skills found".into()));
        }
        candidates
            .iter()
            .map(|candidate| {
                install(candidate, self.destination(destination)).map_err(SdkError::Skill)
            })
            .collect()
    }

    pub fn uninstall(&self, name: &str) -> Result<bool, SdkError> {
        let found = self.discovered.read().expect("skills lock");
        let Some(skill) = found.skills.iter().find(|skill| skill.name == name) else {
            return Ok(false);
        };
        if !skill.dir.starts_with(&self.managed_root)
            && !skill.dir.starts_with(&self.workspace_root)
        {
            return Err(SdkError::Skill(format!(
                "skill is outside SDK-managed roots: {}",
                skill.dir.display()
            )));
        }
        uninstall(&skill.dir).map_err(SdkError::Skill)?;
        Ok(true)
    }

    fn destination(&self, destination: SkillDestination) -> &Path {
        match destination {
            SkillDestination::Managed => &self.managed_root,
            SkillDestination::Workspace => &self.workspace_root,
        }
    }
}
