use super::*;

fn write_skill(root: &Path, directory: &str, name: &str, description: &str) {
    let dir = root.join("skills").join(directory);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\nFollow the workflow.\n"),
    )
    .unwrap();
}

#[test]
fn agent_plugin_loads_valid_skills_and_isolates_invalid_siblings() {
    let tree = TestTree::new("skills");
    let root = tree.plugin();
    write_manifest(&tree, &root, manifest("skill-plugin"));
    write_skill(
        &root,
        "release-notes",
        "release-notes",
        "Draft release notes from changes",
    );
    write_skill(&root, "bad-name", "different-name", "Invalid name mismatch");
    write_skill(&root, "Uppercase", "Uppercase", "Invalid standard name");

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();

    assert_eq!(plugin.skills.skills.len(), 1);
    assert_eq!(plugin.skills.skills[0].name, "release-notes");
    assert_eq!(plugin.skills.skills[0].root, "plugin:skill-plugin");
    assert_eq!(plugin.skills.failures.len(), 2);
    let warnings = warning_text(&plugin);
    assert!(warnings.contains("skills.bad-name"), "{warnings}");
    assert!(warnings.contains("skills.Uppercase"), "{warnings}");
}

#[test]
fn empty_skills_directory_is_a_valid_component() {
    let tree = TestTree::new("empty-skills");
    let root = tree.plugin();
    fs::create_dir(root.join("skills")).unwrap();
    write_manifest(&tree, &root, manifest("empty-skills-plugin"));

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();

    assert!(plugin.skills.skills.is_empty());
    assert!(plugin.skills.failures.is_empty());
    assert!(!warning_text(&plugin).contains("skills"));
}

#[test]
fn standard_optional_frontmatter_fields_are_strictly_validated() {
    let tree = TestTree::new("strict-skill-frontmatter");
    let root = tree.plugin();
    write_manifest(&tree, &root, manifest("strict-skills"));
    let valid = root.join("skills/valid");
    fs::create_dir_all(&valid).unwrap();
    fs::write(
        valid.join("SKILL.md"),
        "---\nname: valid\ndescription: valid skill\nlicense: Apache-2.0\ncompatibility: Requires git\nmetadata:\n  author: example\nallowed-tools: Read Bash(git:*)\n---\n",
    )
    .unwrap();
    for (name, frontmatter) in [
        ("malformed", "name: [unterminated"),
        (
            "bad-license",
            "name: bad-license\ndescription: bad\nlicense: [MIT]",
        ),
        (
            "bad-compatibility",
            "name: bad-compatibility\ndescription: bad\ncompatibility: ''",
        ),
        (
            "bad-metadata",
            "name: bad-metadata\ndescription: bad\nmetadata:\n  version: 1",
        ),
        (
            "bad-tools",
            "name: bad-tools\ndescription: bad\nallowed-tools:\n  - Read",
        ),
    ] {
        let dir = root.join("skills").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), format!("---\n{frontmatter}\n---\n")).unwrap();
    }
    let long_compatibility = root.join("skills/long-compatibility");
    fs::create_dir_all(&long_compatibility).unwrap();
    fs::write(
        long_compatibility.join("SKILL.md"),
        format!(
            "---\nname: long-compatibility\ndescription: bad\ncompatibility: {}\n---\n",
            "x".repeat(501)
        ),
    )
    .unwrap();

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();

    assert_eq!(plugin.skills.skills.len(), 1);
    assert_eq!(plugin.skills.skills[0].name, "valid");
    assert_eq!(plugin.skills.failures.len(), 6);
}

#[test]
fn invalid_skills_component_does_not_disable_mcp() {
    let tree = TestTree::new("invalid-skills-component");
    let root = tree.plugin();
    write_manifest(&tree, &root, manifest("mixed-plugin"));
    fs::write(root.join("skills"), "not a directory").unwrap();
    write_mcp(
        &tree,
        &root,
        json!({ "good": { "type": "stdio", "command": "node" } }),
    );

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();

    assert!(plugin.skills.skills.is_empty());
    assert_eq!(plugin.mcp_servers.len(), 1);
    assert!(warning_text(&plugin).contains("skills"));
}

#[cfg(unix)]
#[test]
fn symlink_escapes_in_plugin_skills_are_isolated() {
    use std::os::unix::fs::symlink;

    let tree = TestTree::new("skill-symlink-escape");
    let root = tree.plugin();
    let outside = tree.path.join("outside-skill");
    fs::create_dir_all(&outside).unwrap();
    fs::write(
        outside.join("SKILL.md"),
        "---\nname: escaped\ndescription: outside\n---\n",
    )
    .unwrap();
    fs::create_dir(root.join("skills")).unwrap();
    symlink(&outside, root.join("skills/escaped")).unwrap();
    write_skill(&root, "inside", "inside", "inside skill");
    write_manifest(&tree, &root, manifest("safe-plugin"));

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();

    assert_eq!(plugin.skills.skills.len(), 1);
    assert_eq!(plugin.skills.skills[0].name, "inside");
    assert!(warning_text(&plugin).contains("skills.escaped"));
}

#[cfg(unix)]
#[test]
fn dangling_skills_symlink_is_reported_as_an_invalid_component() {
    use std::os::unix::fs::symlink;

    let tree = TestTree::new("dangling-skills");
    let root = tree.plugin();
    write_manifest(&tree, &root, manifest("dangling-skills-plugin"));
    symlink(tree.path.join("missing"), root.join("skills")).unwrap();

    let plugin = load_agent_plugin(&root, &tree.path.join("data")).unwrap();

    assert!(plugin.skills.skills.is_empty());
    assert!(warning_text(&plugin).contains("cannot resolve"));
}
