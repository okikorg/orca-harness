//! Apply an accepted proposal as a skill folder, atomically enough to
//! undo: apply only ever creates a new directory, so the snapshot is
//! the record of what was created and undo deletes exactly that.

use std::fs;
use std::path::{Path, PathBuf};

use super::proposal::SkillProposal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub dir: PathBuf,
}

pub fn apply(root: &Path, proposal: &SkillProposal) -> Result<Applied, String> {
    let dir = root.join(&proposal.name);
    if dir.exists() {
        return Err(format!(
            "skill folder already exists: {} — pick another name or remove it first",
            dir.display()
        ));
    }
    let write = |path: &Path, content: &str| -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))
    };
    let result = (|| {
        write(&dir.join("SKILL.md"), &skill_md(proposal))?;
        for script in &proposal.scripts {
            write(&dir.join(&script.path), &script.code)?;
        }
        Ok(Applied { dir: dir.clone() })
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    result
}

pub fn undo(applied: &Applied) -> Result<(), String> {
    fs::remove_dir_all(&applied.dir).map_err(|e| format!("remove {}: {e}", applied.dir.display()))
}

fn skill_md(proposal: &SkillProposal) -> String {
    let mut out = format!(
        "---\nname: {}\ndescription: {}\n---\n\n{}\n",
        proposal.name, proposal.description, proposal.body
    );
    out.push_str(&format!(
        "\n<!-- evidence: {} (session events reviewed by /refine) -->\n",
        proposal.citations.join(", ")
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::super::proposal::{ScriptFile, SkillProposal};
    use super::*;

    fn proposal() -> SkillProposal {
        SkillProposal {
            name: "retry-with-backoff".into(),
            description: "Use jittered backoff.".into(),
            body: "Cap the delay at 30s.".into(),
            scripts: vec![
                ScriptFile {
                    path: "scripts/check_backoff.py".into(),
                    code: "print(1)".into(),
                },
                ScriptFile {
                    path: "scripts/check_cap.py".into(),
                    code: "print(2)".into(),
                },
            ],
            citations: vec!["e03".into(), "e04".into()],
        }
    }

    fn test_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orca-refine-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn apply_writes_skill_md_and_scripts_then_undo_removes_them() {
        let root = test_root("apply");
        let applied = apply(&root, &proposal()).unwrap();

        let md = fs::read_to_string(applied.dir.join("SKILL.md")).unwrap();
        assert!(md.contains("name: retry-with-backoff"));
        assert!(md.contains("description: Use jittered backoff."));
        assert!(md.contains("Cap the delay at 30s."));
        assert!(md.contains("evidence: e03, e04"));
        assert_eq!(
            fs::read_to_string(applied.dir.join("scripts/check_backoff.py")).unwrap(),
            "print(1)"
        );
        assert_eq!(
            fs::read_to_string(applied.dir.join("scripts/check_cap.py")).unwrap(),
            "print(2)"
        );

        undo(&applied).unwrap();
        assert!(!applied.dir.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_refuses_an_existing_folder() {
        let root = test_root("apply-exists");
        fs::create_dir_all(root.join("retry-with-backoff")).unwrap();
        let err = apply(&root, &proposal()).unwrap_err();
        assert!(err.contains("already exists"));
        let _ = fs::remove_dir_all(&root);
    }
}
