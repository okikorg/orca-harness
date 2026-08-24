//! The discovered skills, behind a shared handle. `/skills` toggles one
//! in the config and the worker rescans and rebuilds, so changes apply
//! to the next run — the same shape as extension and MCP toggles.
//!
//! Unlike MCP, a reload is not a diff. Reconnecting an MCP server costs
//! seconds, which is what earns that code its add/removed/changed
//! bookkeeping; re-reading eleven directories costs microseconds, so a
//! reload simply replaces the set. What the diff logic is still needed
//! for is *reporting*: an unchanged, healthy rescan must say nothing,
//! or every toggle would repeat the same lines into the transcript.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use orca_harness_core::Tool;
use orca_harness_tool_extensions::skills::{discover, Discovered, Skill, SkillRoot, SkillTool};

/// What the last scan found for one skill, as the `/skills` overlay
/// renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillState {
    Loaded {
        root: String,
        bytes: u64,
    },
    /// An earlier root supplied this name; this copy is unreachable.
    Shadowed {
        root: String,
        by: String,
    },
    Failed {
        root: String,
        reason: String,
    },
}

/// One row of the overlay: everything on screen comes from here, so the
/// TUI never touches the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub state: SkillState,
    /// The skill's own folder.
    pub dir: PathBuf,
    /// True when the folder sits under a root this session installs
    /// into — the only skills `/skills remove` will delete. A skill
    /// living in the user's `~/.claude/skills` belongs to whatever put
    /// it there.
    pub removable: bool,
}

/// Cloneable handle onto the scanned skills, captured by the agent-build
/// closure and read by the TUI — the same arrangement as `McpServers`.
///
/// The default (empty roots) finds nothing, which is what test harnesses
/// want: discovery never wanders into the developer's real home
/// directory just because a test built an `App`.
#[derive(Clone, Default)]
pub struct Skills {
    roots: Arc<Vec<SkillRoot>>,
    /// Where `/skills add` puts things: beside `config.json`, so a
    /// downloaded skill never lands in the user's repository uninvited.
    managed: Option<Arc<PathBuf>>,
    /// Where `/skills create` puts things, and where `add --here` puts
    /// them: the project's own skill folder.
    project: Option<Arc<PathBuf>>,
    found: Arc<RwLock<Discovered>>,
}

impl Skills {
    /// Roots are passed in, not derived here, so tests can point a scan
    /// at temp directories without mutating process-global env vars.
    pub fn new(
        workspace: &std::path::Path,
        config_dir: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> Self {
        Self {
            roots: Arc::new(orca_harness_tool_extensions::skills::roots(
                workspace,
                config_dir.as_deref(),
                home.as_deref(),
            )),
            managed: config_dir.map(|dir| Arc::new(dir.join("skills"))),
            project: Some(Arc::new(workspace.join(".orca/skills"))),
            found: Arc::default(),
        }
    }

    /// The install root for `/skills add`, and for `create --global`.
    pub fn managed_root(&self) -> Option<&std::path::Path> {
        self.managed.as_deref().map(PathBuf::as_path)
    }

    /// The project's own skill folder: `/skills create` writes here, and
    /// `/skills add --here` installs here.
    pub fn project_root(&self) -> Option<&std::path::Path> {
        self.project.as_deref().map(PathBuf::as_path)
    }

    /// Whether a folder is one this session may delete: only the two
    /// roots it installs into. Anything under `~/.claude/skills` and the
    /// other compatibility roots belongs to whatever put it there.
    fn removable(&self, dir: &std::path::Path) -> bool {
        [self.managed_root(), self.project_root()]
            .into_iter()
            .flatten()
            .any(|root| dir.starts_with(root))
    }

    /// The roots a session scans: the workspace, the config directory
    /// beside `config.json`, and the user's home.
    pub fn for_session(workspace: &std::path::Path) -> Self {
        let config_dir = crate::config::config_path()
            .and_then(|path| path.parent().map(std::path::Path::to_path_buf));
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::new(workspace, config_dir, home)
    }

    /// The `skill` tool over every enabled skill, or `None` when there
    /// is nothing to load — a tool with an empty catalog is worse than
    /// no tool at all.
    pub fn tool(&self) -> Option<Arc<dyn Tool>> {
        let found = self.found.read().expect("skills lock");
        let enabled: Vec<Skill> = found
            .skills
            .iter()
            .filter(|skill| is_enabled(&skill.name))
            .cloned()
            .collect();
        (!enabled.is_empty()).then(|| Arc::new(SkillTool::new(enabled)) as Arc<dyn Tool>)
    }

    /// Every skill the last scan saw — loaded, shadowed, or broken — in
    /// catalog order, for the overlay.
    pub fn catalog(&self) -> Vec<SkillEntry> {
        let found = self.found.read().expect("skills lock");
        let loaded = found.skills.iter().map(|skill| SkillEntry {
            name: skill.name.clone(),
            description: skill.description.clone(),
            enabled: is_enabled(&skill.name),
            state: SkillState::Loaded {
                root: skill.root.clone(),
                bytes: skill.bytes,
            },
            dir: skill.dir.clone(),
            removable: self.removable(&skill.dir),
        });
        let shadowed = found.shadowed.iter().map(|shadow| SkillEntry {
            name: shadow.name.clone(),
            description: String::new(),
            enabled: is_enabled(&shadow.name),
            state: SkillState::Shadowed {
                root: shadow.root.clone(),
                by: shadow.by.clone(),
            },
            dir: shadow.dir.clone(),
            removable: self.removable(&shadow.dir),
        });
        let failed = found.failures.iter().map(|failure| SkillEntry {
            name: failure.name.clone(),
            description: String::new(),
            enabled: is_enabled(&failure.name),
            state: SkillState::Failed {
                root: failure.root.clone(),
                reason: failure.reason.clone(),
            },
            dir: failure.dir.clone(),
            removable: self.removable(&failure.dir),
        });
        loaded.chain(shadowed).chain(failed).collect()
    }

    /// Write a starter `SKILL.md`. Authoring goes to the project folder
    /// by default — a skill you write for this repository belongs in it,
    /// under version control with the code it describes.
    pub fn create(&self, name: &str, global: bool) -> Result<PathBuf, String> {
        let root = self
            .root_for(global)
            .ok_or("no folder to create skills in")?;
        orca_harness_tool_extensions::skills::scaffold(&root, name)
    }

    /// Copy skills in from a folder or a repository. Returns the lines
    /// to print; the caller reloads afterwards.
    ///
    /// The destination is the managed root by default: a skill fetched
    /// from someone else's repository should not silently appear as an
    /// untracked folder in the user's project. `--here` says otherwise.
    pub async fn add(&self, source: &str, here: bool) -> Result<Vec<String>, String> {
        let request = orca_harness_tool_extensions::skills::parse_request(source)?;
        let root = self
            .root_for(!here)
            .ok_or("no folder to install skills into")?;
        let checkout = orca_harness_tool_extensions::skills::checkout(&request.origin).await?;
        let mut candidates = orca_harness_tool_extensions::skills::find_candidates(&checkout.root);
        if let Some(filter) = &request.filter {
            candidates.retain(|candidate| &candidate.name == filter);
            if candidates.is_empty() {
                return Err(format!("no skill named {filter} in that source"));
            }
        }
        if candidates.is_empty() {
            return Err("no SKILL.md found in that source".into());
        }
        if request.list_only {
            let mut lines = vec![format!("{} skill(s) in that source:", candidates.len())];
            lines.extend(
                candidates
                    .iter()
                    .map(|candidate| format!("  {}", candidate.name)),
            );
            lines.push("add one with /skills add <source> --skill <name>".into());
            return Ok(lines);
        }
        let mut lines = Vec::new();
        for candidate in &candidates {
            match orca_harness_tool_extensions::skills::install(candidate, &root) {
                Ok(installed) => lines.push(format!(
                    "installed {} → {}",
                    installed.name,
                    installed.path.display()
                )),
                Err(err) => lines.push(format!("skill {} not installed: {err}", candidate.name)),
            }
        }
        // A skill body is instructions the model will follow, so say so
        // once, here, where the user has just taken someone else's word
        // for what it contains.
        lines
            .push("read what you installed: a skill body is instructions the agent follows".into());
        Ok(lines)
    }

    /// Delete an installed skill, refusing anything this session did not
    /// install. Returns the line to print.
    pub fn remove(&self, name: &str) -> Result<String, String> {
        let entry = self
            .catalog()
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| format!("no skill named {name}"))?;
        if !entry.removable {
            return Err(format!(
                "{name} lives in {} — /skills remove only deletes what it installed; \
                 delete it there instead",
                entry.dir.display()
            ));
        }
        orca_harness_tool_extensions::skills::uninstall(&entry.dir)?;
        // Drop the on/off override too, so a later reinstall of the same
        // name does not come back silently disabled.
        let _ = crate::config::forget_skill(name);
        Ok(format!("removed {name} ({})", entry.dir.display()))
    }

    fn root_for(&self, global: bool) -> Option<PathBuf> {
        match global {
            true => self.managed_root().map(std::path::Path::to_path_buf),
            false => self.project_root().map(std::path::Path::to_path_buf),
        }
    }

    /// Rescan every root, replacing the set. Returns transcript lines
    /// only when the outcome actually changed: a count for the loaded
    /// set, then one line per broken skill. A repeat scan of a healthy,
    /// unchanged tree reports nothing.
    pub fn reload(&self) -> Vec<String> {
        let scanned = discover(&self.roots);
        let mut found = self.found.write().expect("skills lock");
        if *found == scanned {
            return Vec::new();
        }
        let mut lines = Vec::new();
        if found.skills != scanned.skills {
            lines.push(match scanned.skills.len() {
                0 => "skills · none found".to_string(),
                1 => "skills · 1 loaded".to_string(),
                n => format!("skills · {n} loaded"),
            });
        }
        for failure in &scanned.failures {
            lines.push(format!(
                "skill {} ({}) · {}",
                failure.name, failure.root, failure.reason
            ));
        }
        *found = scanned;
        lines
    }
}

/// A skill runs unless the user turned it off; an unknown name has no
/// override and so is on.
fn is_enabled(name: &str) -> bool {
    crate::config::stored_skill_enabled(name).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "orca-cli-skills-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, body: &str) {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }

        fn skills(&self) -> Skills {
            // No config dir and no home: the scan must not be able to
            // reach the developer's real ~/.claude/skills from a test.
            Skills::new(&self.0.join("repo"), None, None)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn skill_md(name: &str) -> String {
        format!("---\nname: {name}\ndescription: does {name}\n---\n\nrun {name}\n")
    }

    /// An empty tree is quiet and registers no tool; a broken skill
    /// reports once and never stops the others.
    #[test]
    fn reload_reports_only_what_changed() {
        let temp = Temp::new("reload");
        let skills = temp.skills();
        assert!(skills.reload().is_empty());
        assert!(skills.tool().is_none());
        assert!(skills.catalog().is_empty());

        temp.write("repo/.orca/skills/release/SKILL.md", &skill_md("release"));
        temp.write("repo/.orca/skills/broken/SKILL.md", "no frontmatter\n");
        let lines = skills.reload();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0], "skills · 1 loaded");
        assert!(lines[1].starts_with("skill broken (.orca/skills) ·"));
        assert!(skills.tool().is_some());

        // Nothing changed: a rescan says nothing at all.
        assert!(skills.reload().is_empty());
    }

    #[test]
    fn disabled_skills_leave_the_tool_but_stay_in_the_catalog() {
        let temp = Temp::new("toggle");
        let skills = temp.skills();
        temp.write("repo/.orca/skills/release/SKILL.md", &skill_md("release"));
        skills.reload();

        assert!(skills.tool().is_some());
        assert!(skills.catalog()[0].enabled);

        crate::config::save_skill_enabled("release", false).unwrap();
        // The only skill is off, so the tool goes away entirely...
        assert!(skills.tool().is_none());
        // ...but the row stays, or there would be no way to turn it on.
        let catalog = skills.catalog();
        assert_eq!(catalog.len(), 1);
        assert!(!catalog[0].enabled);
        assert!(matches!(catalog[0].state, SkillState::Loaded { .. }));
    }

    #[test]
    fn catalog_carries_shadowed_and_failed_rows() {
        let temp = Temp::new("catalog");
        let skills = temp.skills();
        temp.write("repo/.orca/skills/review/SKILL.md", &skill_md("review"));
        temp.write("repo/skills/review/SKILL.md", &skill_md("review"));
        temp.write("repo/skills/oops/SKILL.md", "---\nname: oops\n---\n");
        skills.reload();

        let catalog = skills.catalog();
        assert_eq!(catalog.len(), 3);
        // Two rows share the name: the winner loads, the other is shown
        // as shadowed rather than vanishing.
        assert_eq!(catalog[0].name, "review");
        assert!(
            matches!(&catalog[0].state, SkillState::Loaded { root, .. } if root == ".orca/skills")
        );
        assert_eq!(catalog[1].name, "review");
        assert!(
            matches!(&catalog[1].state, SkillState::Shadowed { root, by } if root == "skills" && by == ".orca/skills")
        );
        assert_eq!(catalog[2].name, "oops");
        assert!(matches!(catalog[2].state, SkillState::Failed { .. }));
    }
}
