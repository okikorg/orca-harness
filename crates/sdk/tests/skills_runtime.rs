//! Skill previews and the run-boundary refresh of the `skill` tool: a
//! preview reads a source without installing from it, and catalog
//! changes reach the next run of a session, never the one in flight.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use orca_harness_core::FnTool;
use orca_harness_sdk::{Harness, SkillDestination, SkillPreview, Skills, ToolPreset};
use serde_json::json;

mod common;
mod schema_support;
use common::temp_dir;
use schema_support::{offers, tool_results, Recording, Step};

fn write_skill(root: &Path, dir: &str, name: &str, description: &str) {
    let folder = root.join(dir);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\nSteps.\n"),
    )
    .unwrap();
}

/// A source folder holding `alpha` and `beta`.
fn source_with_two_skills(root: &Path) -> PathBuf {
    let src = root.join("source");
    write_skill(&src, "alpha", "alpha", "First skill");
    write_skill(&src, "beta", "beta", "Second skill");
    src
}

/// A `Skills` handle over `root` isolated from the real home directory.
fn isolated_skills(root: &Path) -> Skills {
    Skills::new(root, Some(root.join("state")), None)
}

fn managed_root(root: &Path) -> PathBuf {
    root.join("state").join("skills")
}

fn workspace_root(root: &Path) -> PathBuf {
    root.join(".orca").join("skills")
}

fn entries(dir: &Path) -> Vec<String> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn expected_previews(src: &Path) -> Vec<SkillPreview> {
    vec![
        SkillPreview {
            name: "alpha".into(),
            description: "First skill".into(),
            path: PathBuf::from("alpha"),
            origin: src.display().to_string(),
        },
        SkillPreview {
            name: "beta".into(),
            description: "Second skill".into(),
            path: PathBuf::from("beta"),
            origin: src.display().to_string(),
        },
    ]
}

#[tokio::test]
async fn preview_reads_metadata_without_installing() {
    let root = temp_dir("skills-preview");
    let src = source_with_two_skills(&root);
    let skills = isolated_skills(&root);
    let before = skills.catalog();

    let previews = skills.preview(&src.display().to_string()).await.unwrap();

    assert_eq!(previews, expected_previews(&src));
    assert!(entries(&managed_root(&root)).is_empty());
    assert!(entries(&workspace_root(&root)).is_empty());
    assert_eq!(skills.catalog(), before);
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn install_list_only_returns_previews_and_installs_nothing() {
    let root = temp_dir("skills-list-only");
    let src = source_with_two_skills(&root);
    let skills = isolated_skills(&root);

    let outcome = skills
        .install(
            &format!("{} --list", src.display()),
            SkillDestination::Managed,
        )
        .await
        .unwrap();
    assert!(outcome.clone().installed().is_empty());
    assert_eq!(outcome.previewed(), expected_previews(&src));
    assert!(entries(&managed_root(&root)).is_empty());

    let outcome = skills
        .install(&src.display().to_string(), SkillDestination::Managed)
        .await
        .unwrap();
    assert!(outcome.clone().previewed().is_empty());
    let installed = outcome.installed();
    let names: Vec<&str> = installed.iter().map(|item| item.name.as_str()).collect();
    assert_eq!(names, ["alpha", "beta"]);
    assert!(installed
        .iter()
        .all(|item| item.path.starts_with(managed_root(&root))));

    let found = skills.reload();
    let mut names: Vec<&str> = found.skills.iter().map(|s| s.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["alpha", "beta"]);
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn preview_with_filter_narrows_and_missing_source_errors() {
    let root = temp_dir("skills-preview-filter");
    let src = source_with_two_skills(&root);
    let skills = isolated_skills(&root);

    let previews = skills
        .preview(&format!("{} --skill beta", src.display()))
        .await
        .unwrap();
    assert_eq!(previews, expected_previews(&src)[1..]);

    let previews = skills
        .preview(&format!("{} --skill gamma --list", src.display()))
        .await
        .unwrap();
    assert!(previews.is_empty(), "a preview of nothing is not an error");

    let missing = root.join("nowhere");
    assert!(skills
        .preview(&missing.display().to_string())
        .await
        .is_err());
    assert!(entries(&managed_root(&root)).is_empty());
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn skill_changes_apply_at_the_next_run_not_mid_run() {
    let root = temp_dir("skills-run-boundary");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let skills = isolated_skills(&root);
    skills.scaffold("s1", SkillDestination::Workspace).unwrap();
    skills.reload();

    let model = Arc::new(Recording::default());
    let toggled = skills.clone();
    let toggle = FnTool::new(
        "toggle",
        "disables every skill mid-run",
        json!({"type": "object", "properties": {}}),
        move |_, _| {
            let skills = toggled.clone();
            async move {
                skills.disable("s1");
                skills.disable("s2");
                Ok(json!({"disabled": ["s1", "s2"]}))
            }
        },
    );
    let agent = harness
        .agent(model.clone())
        .tools(ToolPreset::None)
        .tool(toggle)
        .skills(skills.clone())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();

    session.run("one").await.unwrap();
    let offered = model.take();
    assert_eq!(offered.len(), 1);
    assert!(offers(&offered[0], "skill"));

    skills.disable("s1");
    session.run("two").await.unwrap();
    let offered = model.take();
    assert!(!offers(&offered[0], "skill"));

    skills.enable("s1");
    skills.scaffold("s2", SkillDestination::Workspace).unwrap();
    skills.reload();
    session.run("three").await.unwrap();
    let offered = model.take();
    assert!(offers(&offered[0], "skill"));

    // Within one run every schema, parameters included, is fixed even
    // when a tool callback changes the catalog between two model calls.
    model.script(vec![Step::Call("toggle", json!({})), Step::Final]);
    session.run("four").await.unwrap();
    let offered = model.take();
    assert_eq!(offered.len(), 2);
    assert_eq!(offered[0], offered[1]);
    assert!(offers(&offered[0], "skill"));

    session.run("five").await.unwrap();
    let offered = model.take();
    assert!(!offers(&offered[0], "skill"));

    // A fresh ephemeral run reads the same catalog.
    skills.enable("s1");
    agent.run("six").await.unwrap();
    let offered = model.take();
    assert!(offers(&offered[0], "skill"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn skill_once_follows_the_run() {
    let root = temp_dir("skills-once");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let skills = isolated_skills(&root);
    skills.scaffold("s1", SkillDestination::Workspace).unwrap();
    skills.reload();

    let model = Arc::new(Recording::default());
    let agent = harness
        .agent(model.clone())
        .tools(ToolPreset::None)
        .skills(skills.clone())
        .build()
        .unwrap();
    let session = agent.new_session().ephemeral().open().unwrap();

    model.script(vec![
        Step::Call("skill", json!({"name": "s1"})),
        Step::Call("skill", json!({"name": "s1"})),
        Step::Final,
    ]);
    let result = session.run("load").await.unwrap();
    let offered = model.take_names();
    assert_eq!(offered.len(), 3);
    for names in &offered {
        let count = names.iter().filter(|name| *name == "skill").count();
        assert_eq!(count, 1, "the skill tool registers once per run");
    }
    let results = tool_results(&result.messages);
    assert_eq!(results.len(), 2);
    assert!(
        results[0].2.get("instructions").is_some(),
        "first load is real"
    );
    assert_eq!(results[1].2["alreadyLoaded"], json!(true));
    std::fs::remove_dir_all(&root).unwrap();
}
