use super::*;
use std::fs as stdfs;

struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "orca-skilltool-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = stdfs::remove_dir_all(&dir);
        stdfs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn write(&self, rel: &str, body: &str) -> PathBuf {
        let path = self.0.join(rel);
        stdfs::create_dir_all(path.parent().unwrap()).unwrap();
        stdfs::write(&path, body).unwrap();
        path
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = stdfs::remove_dir_all(&self.0);
    }
}

fn ctx() -> ToolContext {
    ToolContext {
        call_id: "1".into(),
        tool_name: "skill".into(),
        cancellation: orca_harness_core::CancellationToken::new(),
        deadline: None,
    }
}

fn tool(temp: &Temp) -> SkillTool {
    let found = crate::skills::skill::discover(&crate::skills::skill::roots(
        &temp.0.join("repo"),
        None,
        None,
    ));
    assert!(found.failures.is_empty(), "{:?}", found.failures);
    SkillTool::new(found.skills)
}

fn release(temp: &Temp) {
    temp.write(
        "repo/.orca/skills/release/SKILL.md",
        "---\nname: release\ndescription: Cut a release\n---\n\n1. bump\n2. tag\n",
    );
    temp.write("repo/.orca/skills/release/checklist.md", "- green CI\n");
}

#[tokio::test]
async fn loads_instructions_without_the_frontmatter() {
    let temp = Temp::new("load");
    release(&temp);
    let out = tool(&temp)
        .call(json!({"name": "release"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["instructions"], "1. bump\n2. tag\n");
    assert_eq!(out["name"], "release");
    assert_eq!(out["root"], ".orca/skills");
    assert_eq!(out["resources"], json!(["checklist.md"]));
    assert!(out.get("nextOffset").is_none());
}

#[tokio::test]
async fn reads_a_resource_beside_the_instructions() {
    let temp = Temp::new("resource");
    release(&temp);
    let out = tool(&temp)
        .call(
            json!({"name": "release", "resource": "checklist.md"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out["instructions"], "- green CI\n");
    assert_eq!(out["resource"], "checklist.md");
    // A resource is returned whole, frontmatter rules do not apply.
    assert!(out.get("resources").is_none());
}

/// The containment invariant: neither `..`, nor an absolute path,
/// nor a symlink pointing out of the folder may escape it.
#[tokio::test]
async fn resource_cannot_escape_the_skill_folder() {
    let temp = Temp::new("escape");
    release(&temp);
    temp.write("secret.txt", "token\n");
    let skill = tool(&temp);

    for rel in ["../secret.txt", "../../secret.txt", "/etc/hosts"] {
        let err = skill
            .call(json!({"name": "release", "resource": rel}), &ctx())
            .await
            .expect_err("must refuse");
        assert!(err.to_string().contains("skill folder"), "rel {rel}: {err}");
    }

    #[cfg(unix)]
    {
        let link = temp.0.join("repo/.orca/skills/release/out.txt");
        std::os::unix::fs::symlink(temp.0.join("secret.txt"), &link).unwrap();
        let err = skill
            .call(json!({"name": "release", "resource": "out.txt"}), &ctx())
            .await
            .expect_err("a symlink out of the folder is still out of the folder");
        assert!(err.to_string().contains("skill folder"), "{err}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn resource_listing_does_not_follow_escaping_directory_symlinks() {
    let temp = Temp::new("listing-escape");
    release(&temp);
    temp.write("outside/private-name.txt", "secret\n");
    std::os::unix::fs::symlink(
        temp.0.join("outside"),
        temp.0.join("repo/.orca/skills/release/escaped"),
    )
    .unwrap();

    let out = tool(&temp)
        .call(json!({"name": "release"}), &ctx())
        .await
        .unwrap();

    assert_eq!(out["resources"], json!(["checklist.md"]));
}

/// A file symlink that leaves the folder is not listed either, while
/// one that resolves inside it still is: only symlinks are followed
/// to decide, since a plain entry cannot leave the folder it is in.
#[cfg(unix)]
#[tokio::test]
async fn resource_listing_follows_symlinks_only_inside_the_folder() {
    let temp = Temp::new("listing-file-symlink");
    release(&temp);
    temp.write("outside/private-name.txt", "secret\n");
    let skill_dir = temp.0.join("repo/.orca/skills/release");
    std::os::unix::fs::symlink(
        temp.0.join("outside/private-name.txt"),
        skill_dir.join("leaked.txt"),
    )
    .unwrap();
    std::os::unix::fs::symlink(skill_dir.join("checklist.md"), skill_dir.join("alias.md")).unwrap();

    let out = tool(&temp)
        .call(json!({"name": "release"}), &ctx())
        .await
        .unwrap();

    assert_eq!(out["resources"], json!(["alias.md", "checklist.md"]));
}

#[tokio::test]
async fn long_instructions_page_with_next_offset() {
    let temp = Temp::new("paging");
    let body = "x".repeat(25);
    temp.write(
        "repo/.orca/skills/long/SKILL.md",
        &format!("---\nname: long\ndescription: long one\n---\n{body}"),
    );
    let skill = tool(&temp).chunk(10);

    let first = skill.call(json!({"name": "long"}), &ctx()).await.unwrap();
    assert_eq!(first["instructions"], "x".repeat(10));
    assert_eq!(first["nextOffset"], 10);

    let last = skill
        .call(json!({"name": "long", "offset": 20}), &ctx())
        .await
        .unwrap();
    assert_eq!(last["instructions"], "x".repeat(5));
    assert!(last.get("nextOffset").is_none());

    // Past the end is empty, not an error: a model that pages once
    // too often gets a clean stop.
    let past = skill
        .call(json!({"name": "long", "offset": 999}), &ctx())
        .await
        .unwrap();
    assert_eq!(past["instructions"], "");
}

#[tokio::test]
async fn unknown_name_names_the_alternatives() {
    let temp = Temp::new("unknown");
    release(&temp);
    let err = tool(&temp)
        .call(json!({"name": "nope"}), &ctx())
        .await
        .expect_err("unknown skill");
    assert!(err.to_string().contains("available: release"), "{err}");
}

/// The body is read per call, so editing a skill mid-session works
/// without a reload; only the catalog needs one.
#[tokio::test]
async fn body_reflects_an_edit_made_after_discovery() {
    let temp = Temp::new("edit");
    release(&temp);
    let skill = tool(&temp);
    temp.write(
        "repo/.orca/skills/release/SKILL.md",
        "---\nname: release\ndescription: Cut a release\n---\nrewritten\n",
    );
    let out = skill
        .call(json!({"name": "release"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["instructions"], "rewritten\n");
}

/// The catalog rides in the schema on every turn, and a user's whole
/// `~/.claude/skills` collection can land in it at once, so each
/// entry is bounded. The full description is one call away.
#[test]
fn catalog_entries_are_clipped() {
    let temp = Temp::new("clip");
    let long = "word ".repeat(80);
    temp.write(
        "repo/.orca/skills/verbose/SKILL.md",
        &format!("---\nname: verbose\ndescription: {long}\n---\nbody\n"),
    );
    let description = tool(&temp).schema().description;
    let line = description
        .lines()
        .find(|line| line.trim_start().starts_with("verbose"))
        .expect("catalog line");
    assert!(line.ends_with('…'), "{line}");
    assert!(line.chars().count() < 200, "{}", line.chars().count());
}

#[test]
fn schema_carries_the_catalog_and_the_enum() {
    let temp = Temp::new("schema");
    release(&temp);
    let schema = tool(&temp).schema();
    assert_eq!(schema.name, "skill");
    assert!(schema.description.contains("release — Cut a release"));
    assert_eq!(
        schema.parameters["properties"]["name"]["enum"],
        json!(["release"])
    );
}

/// The model reads this description fresh every turn, and "call this
/// before starting work" alone reads as "again, this turn". The
/// description has to say what `SkillOnce` enforces.
#[test]
fn description_says_a_skill_loads_once() {
    let temp = Temp::new("once");
    release(&temp);
    let description = tool(&temp).schema().description;
    assert!(
        description.contains("once per conversation"),
        "{description}"
    );
    assert!(description.contains("do not reload"), "{description}");
}
