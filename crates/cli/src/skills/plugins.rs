//! Enabled Agent Plugin Skills captured once for one Orcacode process.

use std::path::PathBuf;

use orca_harness_tool_extensions::skills::{Discovered, Shadowed};

impl super::Skills {
    pub(crate) fn loaded_count(&self) -> usize {
        self.found.read().expect("skills lock").skills.len()
    }
}

pub(super) fn enabled(skill: &orca_harness_tool_extensions::skills::Skill) -> bool {
    skill.root.starts_with("plugin:") || super::is_enabled(&skill.name)
}

pub(super) fn load(
    mut registrations: Vec<crate::config::RegisteredPlugin>,
    data_path: impl Fn(&str) -> std::io::Result<PathBuf>,
) -> Discovered {
    registrations.sort_by(|left, right| left.name.cmp(&right.name));
    let mut found = Discovered::default();
    for registered in registrations.into_iter().filter(|plugin| plugin.enabled) {
        let Ok(data) = data_path(&registered.name) else {
            continue;
        };
        let Ok(plugin) =
            orca_harness_tool_extensions::agent_plugins::load_agent_plugin(&registered.root, &data)
        else {
            continue;
        };
        if plugin.name == registered.name {
            merge(&mut found, plugin.skills);
        }
    }
    found
}

pub(super) fn merge(target: &mut Discovered, incoming: Discovered) {
    for skill in incoming.skills {
        match target
            .skills
            .iter()
            .find(|loaded| loaded.name == skill.name)
        {
            Some(winner) => target.shadowed.push(Shadowed {
                name: skill.name,
                root: skill.root,
                by: winner.root.clone(),
                dir: skill.dir,
            }),
            None => target.skills.push(skill),
        }
    }
    target.shadowed.extend(incoming.shadowed);
    target.failures.extend(incoming.failures);
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use orca_harness_core::{CancellationToken, ToolContext};
    use serde_json::json;

    use super::*;
    use crate::skills::{SkillState, Skills};

    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "orca-plugin-skills-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, relative: &str, body: &str) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }

        fn plugin(
            &self,
            name: &str,
            skill: &str,
            enabled: bool,
        ) -> crate::config::RegisteredPlugin {
            self.write(
                &format!("{name}/plugin.json"),
                &format!(
                    "{{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\"name\":\"{name}\"}}"
                ),
            );
            self.write(&format!("{name}/skills/{skill}/SKILL.md"), &skill_md(skill));
            crate::config::RegisteredPlugin {
                name: name.into(),
                root: self.0.join(name),
                enabled,
            }
        }

        fn skills(&self) -> Skills {
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

    #[tokio::test]
    async fn enabled_skills_join_the_tool_and_disabled_ones_do_not() {
        let temp = Temp::new("visibility");
        let enabled = temp.plugin("release-plugin", "draft-release-notes", true);
        let disabled = temp.plugin("incident-plugin", "triage-incident", false);
        let mut skills = temp.skills();
        skills.plugin_found = Arc::new(load(vec![disabled, enabled], |name| {
            Ok(temp.0.join("data").join(name))
        }));

        assert_eq!(skills.reload(), ["skills · 1 loaded"]);
        assert_eq!(skills.loaded_count(), 1);
        assert_eq!(skills.plugin_count("release-plugin"), 1);
        assert_eq!(skills.plugin_count("incident-plugin"), 0);
        assert!(!skills.is_invokable("triage-incident"));

        crate::config::save_skill_enabled("draft-release-notes", false).unwrap();
        assert!(skills.is_invokable("draft-release-notes"));
        assert!(
            skills.catalog()[0].enabled,
            "plugin enablement owns its Skills"
        );

        let tool = skills.tool().expect("plugin skill tool");
        let result = tool
            .call(
                json!({"name": "draft-release-notes"}),
                &ToolContext {
                    call_id: "plugin-skill-test".into(),
                    tool_name: "skill".into(),
                    cancellation: CancellationToken::new(),
                    deadline: None,
                },
            )
            .await
            .unwrap();
        assert!(result["instructions"]
            .as_str()
            .unwrap()
            .contains("run draft-release-notes"));
    }

    #[test]
    fn workspace_skill_shadows_same_named_plugin_skill() {
        let temp = Temp::new("shadow");
        let enabled = temp.plugin("review-plugin", "review", true);
        temp.write("repo/.orca/skills/review/SKILL.md", &skill_md("review"));
        let mut skills = temp.skills();
        skills.plugin_found = Arc::new(load(vec![enabled], |name| {
            Ok(temp.0.join("data").join(name))
        }));

        skills.reload();

        let catalog = skills.catalog();
        assert!(matches!(
            &catalog[0].state,
            SkillState::Loaded { root, .. } if root == ".orca/skills"
        ));
        assert!(matches!(
            &catalog[1].state,
            SkillState::Shadowed { root, by }
                if root == "plugin:review-plugin" && by == ".orca/skills"
        ));
        assert_eq!(skills.plugin_count("review-plugin"), 0);
    }
}
