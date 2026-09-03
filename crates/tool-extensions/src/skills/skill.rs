//! Discovery and frontmatter parsing.
//!
//! The format is deliberately a narrow subset of what a YAML parser
//! would accept — the workspace carries no YAML crate and this is not
//! worth one. What it *must* handle is what real files in the wild use:
//! block-scalar descriptions (`|`, `>`), quoted values, and unknown keys
//! that belong to some other agent's schema.

use std::fs;
use std::path::{Path, PathBuf};

/// Workspace-relative directories scanned for skills, in precedence
/// order. The first two are ours; the rest are compatibility roots, so a
/// repository that already carries skills for another agent works here
/// with nothing moved.
pub const WORKSPACE_ROOTS: &[&str] = &[
    ".orca/skills",
    "skills",
    ".claude/skills",
    ".agents/skills",
    ".codex/skills",
    ".opencode/skills",
];

/// Home-relative compatibility roots, scanned after the config-dir root.
pub const HOME_ROOTS: &[&str] = &[
    ".claude/skills",
    ".agents/skills",
    ".codex/skills",
    ".config/opencode/skills",
];

/// One directory to scan. `label` is what the UI shows and what a
/// shadowing report names, so it is the short form (`.orca/skills`,
/// `~/.claude/skills`), not the absolute path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillRoot {
    pub label: String,
    pub path: PathBuf,
}

/// The roots for one session, in precedence order: the workspace first
/// (a project's own skills win), then the user's.
///
/// Paths are passed in rather than read from the environment: this is
/// what lets tests point discovery at temp directories without mutating
/// process-global env vars.
pub fn roots(workspace: &Path, config_dir: Option<&Path>, home: Option<&Path>) -> Vec<SkillRoot> {
    let mut roots: Vec<SkillRoot> = WORKSPACE_ROOTS
        .iter()
        .map(|rel| SkillRoot {
            label: (*rel).to_string(),
            path: workspace.join(rel),
        })
        .collect();
    if let Some(dir) = config_dir {
        roots.push(SkillRoot {
            label: "config/skills".to_string(),
            path: dir.join("skills"),
        });
    }
    if let Some(home) = home {
        roots.extend(HOME_ROOTS.iter().map(|rel| SkillRoot {
            label: format!("~/{rel}"),
            path: home.join(rel),
        }));
    }
    roots
}

/// One loaded skill. `dir` is the containment boundary for resource
/// reads; `file` is the `SKILL.md` inside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub file: PathBuf,
    /// Label of the root it came from.
    pub root: String,
    /// Size of `SKILL.md`, for the picker.
    pub bytes: u64,
}

/// A skill that parsed but lost a name collision to an earlier root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shadowed {
    pub name: String,
    pub root: String,
    /// Label of the root whose copy won.
    pub by: String,
    /// The unreachable copy's own folder, so the UI can say where it is
    /// and an uninstall can find it.
    pub dir: PathBuf,
}

/// A `SKILL.md` that could not be loaded. Reported and skipped — one bad
/// skill never costs the others, exactly as one MCP server that will not
/// connect never costs the others.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillFailure {
    /// Directory name; the frontmatter may be the very thing that failed.
    pub name: String,
    pub root: String,
    pub reason: String,
    /// The folder it was found in — a broken skill still needs to be
    /// findable, and removable.
    pub dir: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovered {
    pub skills: Vec<Skill>,
    pub shadowed: Vec<Shadowed>,
    pub failures: Vec<SkillFailure>,
}

/// Scan `roots` in order. A missing root is not an error (most users
/// have none of the compatibility directories); a directory without a
/// `SKILL.md` is not a skill and is passed over in silence.
pub fn discover(roots: &[SkillRoot]) -> Discovered {
    let mut found = Discovered::default();
    for root in roots {
        let Ok(entries) = fs::read_dir(&root.path) else {
            continue;
        };
        // read_dir order is filesystem order; sort so the catalog — and
        // therefore the tool schema, and therefore the prompt prefix —
        // is stable between runs.
        //
        // The entry type comes with the listing; only a symlink (a skill
        // folder linked in from a dotfiles repo, say) needs a `stat` to
        // learn what it points at.
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .filter(|entry| match entry.file_type() {
                Ok(kind) if !kind.is_symlink() => kind.is_dir(),
                _ => entry.path().is_dir(),
            })
            .map(|entry| entry.path())
            .collect();
        dirs.sort();
        for dir in dirs {
            let file = dir.join("SKILL.md");
            if !file.is_file() {
                continue;
            }
            let dir_name = dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            match load(&dir, &file, &dir_name, &root.label) {
                Ok(skill) => match found.skills.iter().find(|s| s.name == skill.name) {
                    Some(winner) => found.shadowed.push(Shadowed {
                        name: skill.name,
                        root: root.label.clone(),
                        by: winner.root.clone(),
                        dir: skill.dir,
                    }),
                    None => found.skills.push(skill),
                },
                Err(reason) => found.failures.push(SkillFailure {
                    name: dir_name,
                    root: root.label.clone(),
                    reason,
                    dir: dir.clone(),
                }),
            }
        }
    }
    found
}

fn load(dir: &Path, file: &Path, dir_name: &str, root: &str) -> Result<Skill, String> {
    let text = fs::read_to_string(file).map_err(|e| format!("unreadable: {e}"))?;
    // Read whole, so the text's length is the file's size: no `stat`.
    let bytes = text.len() as u64;
    let front = parse_frontmatter(&text)?;
    let name = front.name.unwrap_or_else(|| dir_name.to_string());
    validate_name(&name)?;
    let description = front
        .description
        .filter(|value| !value.is_empty())
        .ok_or("no `description` in the frontmatter")?;
    Ok(Skill {
        name,
        description,
        dir: dir.to_path_buf(),
        file: file.to_path_buf(),
        root: root.to_string(),
        bytes,
    })
}

/// The name is what the model passes back, so it must survive a JSON
/// enum and a picker row: no whitespace, no surprises.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err(format!("unusable skill name: {name:?}"));
    }
    let usable = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !usable || !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err(format!(
            "unusable skill name: {name:?} — letters, digits, -, _ and . only"
        ));
    }
    Ok(())
}

/// The recognized frontmatter fields. Everything else a file carries is
/// parsed past and dropped, so a skill written for another agent loads
/// here instead of erroring.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
}

/// Split the `---` fenced header off a `SKILL.md` and read `name` and
/// `description` out of it.
///
/// `description` is collapsed to a single line: it becomes one row in a
/// picker and one line in the model-facing catalog, and block scalars
/// are common enough in the compatibility roots that folding them is not
/// optional.
pub fn parse_frontmatter(text: &str) -> Result<Frontmatter, String> {
    let text = text.trim_start_matches('\u{feff}');
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or("no `---` frontmatter at the top of the file")?;
    let end = rest
        .lines()
        .position(|line| line.trim_end() == "---")
        .ok_or("frontmatter is never closed by a `---` line")?;

    let mut front = Frontmatter::default();
    // The key whose block scalar we are inside, and whether it folds
    // (`>`) or keeps newlines (`|`).
    let mut block: Option<(String, bool, Vec<String>)> = None;
    let finish = |block: Option<(String, bool, Vec<String>)>, front: &mut Frontmatter| {
        if let Some((key, fold, lines)) = block {
            let joined = if fold {
                lines.join(" ")
            } else {
                lines.join("\n")
            };
            set_field(front, &key, joined);
        }
    };

    for line in rest.lines().take(end) {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if let Some((_, _, lines)) = block.as_mut() {
            if indented || line.trim().is_empty() {
                lines.push(line.trim().to_string());
                continue;
            }
            finish(block.take(), &mut front);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match block_indicator(value) {
            Some(fold) => block = Some((key, fold, Vec::new())),
            None => set_field(&mut front, &key, unquote(value).to_string()),
        }
    }
    finish(block.take(), &mut front);

    front.description = front.description.map(|value| collapse(&value));
    front.name = front.name.map(|value| value.trim().to_string());
    Ok(front)
}

/// `|`, `>`, and their chomping variants start a block scalar; `>` folds
/// to spaces, `|` keeps line breaks.
fn block_indicator(value: &str) -> Option<bool> {
    match value {
        "|" | "|-" | "|+" => Some(false),
        ">" | ">-" | ">+" => Some(true),
        _ => None,
    }
}

fn set_field(front: &mut Frontmatter, key: &str, value: String) {
    match key {
        "name" => front.name = Some(value),
        "description" => front.description = Some(value),
        _ => {}
    }
}

fn unquote(value: &str) -> &str {
    let quoted = |q: char| value.len() >= 2 && value.starts_with(q) && value.ends_with(q);
    if quoted('"') || quoted('\'') {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Whitespace runs (including newlines) become single spaces.
fn collapse(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn front(text: &str) -> Frontmatter {
        parse_frontmatter(text).expect("parses")
    }

    #[test]
    fn reads_name_and_description() {
        let parsed = front("---\nname: release\ndescription: Cut a release\n---\n\nbody\n");
        assert_eq!(parsed.name.as_deref(), Some("release"));
        assert_eq!(parsed.description.as_deref(), Some("Cut a release"));
    }

    /// Files written for other agents carry keys we do not know and
    /// descriptions written as block scalars. Both must load.
    #[test]
    fn tolerates_unknown_keys_quotes_and_block_scalars() {
        let parsed = front(
            "---\n\
             name: \"review\"\n\
             license: MIT\n\
             allowed-tools: [Read, Bash]\n\
             description: >-\n\
             \x20 Use when reviewing a pull request,\n\
             \x20 especially a large one.\n\
             ---\nbody\n",
        );
        assert_eq!(parsed.name.as_deref(), Some("review"));
        assert_eq!(
            parsed.description.as_deref(),
            Some("Use when reviewing a pull request, especially a large one.")
        );

        // `|` keeps its line breaks, but the catalog wants one line, so
        // the result is collapsed either way.
        let literal = front("---\nname: a\ndescription: |\n  first\n  second\n---\n");
        assert_eq!(literal.description.as_deref(), Some("first second"));
    }

    #[test]
    fn rejects_missing_or_unclosed_frontmatter() {
        assert!(parse_frontmatter("# just markdown\n").is_err());
        assert!(parse_frontmatter("---\nname: a\nbody with no fence\n").is_err());
    }

    #[test]
    fn names_must_be_usable() {
        assert!(validate_name("release").is_ok());
        assert!(validate_name("code-review.v2").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("two words").is_err());
        assert!(validate_name("-leading").is_err());
        assert!(validate_name(&"x".repeat(65)).is_err());
    }

    /// A temp workspace with `.orca/skills/<name>/SKILL.md` files.
    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "orca-skills-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, body: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            path
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

    #[test]
    fn discovers_across_roots_in_precedence_order() {
        let temp = Temp::new("discover");
        let ws = temp.0.join("repo");
        let home = temp.0.join("home");
        temp.write("repo/.orca/skills/release/SKILL.md", &skill_md("release"));
        temp.write("repo/.claude/skills/review/SKILL.md", &skill_md("review"));
        temp.write("home/.claude/skills/tidy/SKILL.md", &skill_md("tidy"));

        let found = discover(&roots(&ws, None, Some(&home)));
        let names: Vec<&str> = found.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["release", "review", "tidy"]);
        assert_eq!(found.skills[0].root, ".orca/skills");
        assert_eq!(found.skills[2].root, "~/.claude/skills");
        assert!(found.failures.is_empty());
        assert!(found.shadowed.is_empty());
        // Absent roots are ordinary, not failures.
        assert!(discover(&roots(&temp.0.join("nowhere"), None, None))
            .skills
            .is_empty());
    }

    /// A skill folder that is a symlink (linked in from a dotfiles repo)
    /// is a skill folder; a symlink to a file is not.
    #[cfg(unix)]
    #[test]
    fn symlinked_skill_folders_are_discovered() {
        let temp = Temp::new("symlink");
        let ws = temp.0.join("repo");
        temp.write("elsewhere/linked/SKILL.md", &skill_md("linked"));
        temp.write("elsewhere/stray.md", "not a folder\n");
        let root = ws.join(".orca/skills");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(temp.0.join("elsewhere/linked"), root.join("linked")).unwrap();
        std::os::unix::fs::symlink(temp.0.join("elsewhere/stray.md"), root.join("stray")).unwrap();

        let found = discover(&roots(&ws, None, None));
        let names: Vec<&str> = found.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["linked"]);
        assert!(found.failures.is_empty());
    }

    #[test]
    fn later_roots_are_shadowed_not_dropped_silently() {
        let temp = Temp::new("shadow");
        let ws = temp.0.join("repo");
        let home = temp.0.join("home");
        temp.write("repo/.orca/skills/review/SKILL.md", &skill_md("review"));
        temp.write("home/.claude/skills/review/SKILL.md", &skill_md("review"));

        let found = discover(&roots(&ws, None, Some(&home)));
        assert_eq!(found.skills.len(), 1);
        assert_eq!(found.skills[0].root, ".orca/skills");
        assert_eq!(
            found.shadowed,
            [Shadowed {
                name: "review".into(),
                root: "~/.claude/skills".into(),
                by: ".orca/skills".into(),
                dir: home.join(".claude/skills/review"),
            }]
        );
    }

    #[test]
    fn a_broken_skill_is_reported_and_the_others_still_load() {
        let temp = Temp::new("broken");
        let ws = temp.0.join("repo");
        temp.write("repo/.orca/skills/good/SKILL.md", &skill_md("good"));
        temp.write("repo/.orca/skills/bare/SKILL.md", "no frontmatter here\n");
        temp.write(
            "repo/.orca/skills/nodesc/SKILL.md",
            "---\nname: nodesc\n---\nbody\n",
        );
        // A directory that is simply not a skill is passed over.
        temp.write("repo/.orca/skills/notes/README.md", "hello\n");

        let found = discover(&roots(&ws, None, None));
        assert_eq!(
            found.skills.iter().map(|s| &s.name).collect::<Vec<_>>(),
            ["good"]
        );
        let failed: Vec<&str> = found.failures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(failed, ["bare", "nodesc"]);
        assert!(found.failures[1].reason.contains("description"));
    }

    /// Frontmatter with no `name` takes the directory's, matching the
    /// files other agents ship.
    #[test]
    fn name_falls_back_to_the_directory() {
        let temp = Temp::new("dirname");
        let ws = temp.0.join("repo");
        temp.write(
            "repo/.orca/skills/from-dir/SKILL.md",
            "---\ndescription: named by its folder\n---\nbody\n",
        );
        let found = discover(&roots(&ws, None, None));
        assert_eq!(found.skills[0].name, "from-dir");
    }
}
