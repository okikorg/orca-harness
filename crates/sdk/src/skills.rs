use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use orca_harness_core::Tool;
use orca_harness_tool_extensions::skills::{
    checkout, discover, find_candidates, install, parse_frontmatter, parse_request, scaffold,
    uninstall, Candidate, Checkout, Discovered, Installed, Origin, Skill, SkillRoot, SkillTool,
};

use crate::SdkError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDestination {
    Managed,
    Workspace,
}

/// One skill a source holds, read without installing it. Owned data:
/// nothing here points into the temporary checkout the source was read
/// from, which is gone by the time this is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPreview {
    /// The folder name, which is also what `--skill` filters on and what
    /// an install names the copy.
    pub name: String,
    /// The frontmatter `description`, or empty when the file has none.
    pub description: String,
    /// The skill folder relative to the checkout root: the local folder,
    /// the repository root, or the deep link's subdirectory.
    pub path: PathBuf,
    /// Display form of the source it was read from.
    pub origin: String,
}

/// What [`Skills::install`] did with a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSourceOutcome {
    /// The request carried `--list`: the source was read and nothing was
    /// written.
    Previewed(Vec<SkillPreview>),
    /// The skills copied into the destination.
    Installed(Vec<Installed>),
}

impl SkillSourceOutcome {
    /// The installed skills; empty when the source was only previewed.
    pub fn installed(self) -> Vec<Installed> {
        match self {
            Self::Installed(installed) => installed,
            Self::Previewed(_) => Vec::new(),
        }
    }

    /// The previewed skills; empty when the source was installed.
    pub fn previewed(self) -> Vec<SkillPreview> {
        match self {
            Self::Previewed(previews) => previews,
            Self::Installed(_) => Vec::new(),
        }
    }
}

/// The skill catalog an agent offers through its `skill` tool.
///
/// Clones share one catalog and one enablement set. `enable`, `disable`,
/// and `reload` take effect at the next run of any session built from an
/// agent holding this handle: the tool set, and so the `skill` tool's
/// schema, is fixed for the length of a run and read again at the next
/// run boundary. `scaffold` and `install` change disk and reach the
/// catalog at the next `reload`; `uninstall` changes disk and drops the
/// entry from the catalog at once.
#[derive(Clone)]
pub struct Skills {
    roots: Arc<Vec<SkillRoot>>,
    managed_root: PathBuf,
    workspace_root: PathBuf,
    disabled: Arc<RwLock<HashSet<String>>>,
    discovered: Arc<RwLock<Discovered>>,
    additional: Arc<Discovered>,
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
            additional: Arc::default(),
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
            additional: Arc::default(),
        }
    }

    /// Add a fixed discovery snapshot behind ordinary roots. Reload rescans the
    /// roots but retains this snapshot, including its shadow/failure diagnostics.
    /// This starts an independent catalog and enablement handle.
    pub fn with_additional_skills(mut self, additional: Discovered) -> Self {
        self.additional = Arc::new(additional);
        self.discovered = Arc::default();
        self.disabled = Arc::default();
        self
    }

    pub fn reload(&self) -> Discovered {
        let mut found = discover(&self.roots);
        for skill in &self.additional.skills {
            if let Some(winner) = found.skills.iter().find(|s| s.name == skill.name) {
                found
                    .shadowed
                    .push(orca_harness_tool_extensions::skills::Shadowed {
                        name: skill.name.clone(),
                        root: skill.root.clone(),
                        by: winner.root.clone(),
                        dir: skill.dir.clone(),
                    });
            } else {
                found.skills.push(skill.clone());
            }
        }
        found.shadowed.extend(self.additional.shadowed.clone());
        found.failures.extend(self.additional.failures.clone());
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

    /// Read what `source` holds without installing any of it. `source`
    /// takes the same syntax as [`install`](Self::install) (a `--list`
    /// flag is accepted and ignored; `--skill` narrows the result). A
    /// source holding no matching skill previews as empty rather than
    /// failing; a source that cannot be read is an error.
    pub async fn preview(&self, source: &str) -> Result<Vec<SkillPreview>, SdkError> {
        let request = parse_request(source).map_err(SdkError::Skill)?;
        let checkout = checkout(&request.origin).await.map_err(SdkError::Skill)?;
        let candidates = matching_candidates(&checkout, request.filter.as_deref());
        Ok(previews(&checkout, &candidates, &request.origin))
    }

    /// Copy the skills `source` holds into `destination`. With `--list`
    /// in the request the source is only read, exactly as
    /// [`preview`](Self::preview) reads it, and the outcome is
    /// [`SkillSourceOutcome::Previewed`]; otherwise every matching skill
    /// is copied and the outcome is [`SkillSourceOutcome::Installed`].
    /// An install of nothing is an error; a preview of nothing is not.
    pub async fn install(
        &self,
        source: &str,
        destination: SkillDestination,
    ) -> Result<SkillSourceOutcome, SdkError> {
        let request = parse_request(source).map_err(SdkError::Skill)?;
        let checkout = checkout(&request.origin).await.map_err(SdkError::Skill)?;
        let candidates = matching_candidates(&checkout, request.filter.as_deref());
        if request.list_only {
            return Ok(SkillSourceOutcome::Previewed(previews(
                &checkout,
                &candidates,
                &request.origin,
            )));
        }
        if candidates.is_empty() {
            return Err(SdkError::Skill("no matching skills found".into()));
        }
        candidates
            .iter()
            .map(|candidate| {
                install(candidate, self.destination(destination)).map_err(SdkError::Skill)
            })
            .collect::<Result<Vec<_>, _>>()
            .map(SkillSourceOutcome::Installed)
    }

    /// Delete a catalogued skill's folder and drop it from the catalog,
    /// so the next run does not offer a skill that is no longer on disk.
    /// `Ok(false)` when the catalog holds no such skill.
    pub fn uninstall(&self, name: &str) -> Result<bool, SdkError> {
        let mut found = self.discovered.write().expect("skills lock");
        let Some(index) = found.skills.iter().position(|skill| skill.name == name) else {
            return Ok(false);
        };
        let dir = &found.skills[index].dir;
        if !dir.starts_with(&self.managed_root) && !dir.starts_with(&self.workspace_root) {
            return Err(SdkError::Skill(format!(
                "skill is outside SDK-managed roots: {}",
                dir.display()
            )));
        }
        uninstall(dir).map_err(SdkError::Skill)?;
        found.skills.remove(index);
        Ok(true)
    }

    fn destination(&self, destination: SkillDestination) -> &Path {
        match destination {
            SkillDestination::Managed => &self.managed_root,
            SkillDestination::Workspace => &self.workspace_root,
        }
    }
}

/// The source's skills, narrowed to `filter` when one was given.
fn matching_candidates(checkout: &Checkout, filter: Option<&str>) -> Vec<Candidate> {
    let mut candidates = find_candidates(&checkout.root);
    if let Some(filter) = filter {
        candidates.retain(|candidate| candidate.name == filter);
    }
    candidates
}

/// Owned previews of `candidates`, read while `checkout` is still on
/// disk. A `SKILL.md` whose frontmatter is missing or unreadable
/// previews with an empty description: the report is of what the
/// source holds, and discovery reports the failure after an install.
fn previews(checkout: &Checkout, candidates: &[Candidate], origin: &Origin) -> Vec<SkillPreview> {
    let origin = match origin {
        Origin::Local(path) => path.display().to_string(),
        Origin::Git { url, subdir: None } => url.clone(),
        Origin::Git {
            url,
            subdir: Some(subdir),
        } => format!("{url} ({subdir})"),
    };
    candidates
        .iter()
        .map(|candidate| {
            let description = std::fs::read_to_string(candidate.dir.join("SKILL.md"))
                .ok()
                .and_then(|text| parse_frontmatter(&text).ok())
                .and_then(|front| front.description)
                .unwrap_or_default();
            // Candidates are found by walking down from `checkout.root`,
            // so the prefix always strips; the fallback is unreachable.
            let path = candidate
                .dir
                .strip_prefix(&checkout.root)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| PathBuf::from(&candidate.name));
            SkillPreview {
                name: candidate.name.clone(),
                description,
                path,
                origin: origin.clone(),
            }
        })
        .collect()
}
