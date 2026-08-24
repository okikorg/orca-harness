use super::*;

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "orca-install-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn skill_md(name: &str) -> String {
    format!("---\nname: {name}\ndescription: does {name}\n---\n\nrun {name}\n")
}

#[test]
fn parses_the_shapes_the_ecosystem_cli_accepts() {
    let git = |url: &str| Origin::Git {
        url: url.into(),
        subdir: None,
    };

    let plain = parse_request("vercel-labs/agent-skills").unwrap();
    assert_eq!(
        plain.origin,
        git("https://github.com/vercel-labs/agent-skills.git")
    );
    assert_eq!(plain.filter, None);

    // `owner/repo@skill` names the skill; so does `--skill`.
    assert_eq!(
        parse_request("owner/repo@review")
            .unwrap()
            .filter
            .as_deref(),
        Some("review")
    );
    assert_eq!(
        parse_request("owner/repo --skill review")
            .unwrap()
            .filter
            .as_deref(),
        Some("review")
    );
    assert_eq!(
        parse_request("owner/repo --skill=review")
            .unwrap()
            .filter
            .as_deref(),
        Some("review")
    );

    // A pasted install command from a README works verbatim, flags
    // that mean nothing here included.
    let pasted = parse_request("npx skills add owner/repo -a claude-code --skill x -y").unwrap();
    assert_eq!(pasted.origin, git("https://github.com/owner/repo.git"));
    assert_eq!(pasted.filter.as_deref(), Some("x"));

    // skills.sh links and GitHub deep links.
    assert_eq!(
        parse_request("https://skills.sh/owner/repo")
            .unwrap()
            .origin,
        git("https://github.com/owner/repo.git")
    );
    assert_eq!(
        parse_request("https://github.com/owner/repo/tree/main/skills/review")
            .unwrap()
            .origin,
        Origin::Git {
            url: "https://github.com/owner/repo.git".into(),
            subdir: Some("skills/review".into()),
        }
    );
    assert_eq!(
        parse_request("git@github.com:owner/repo.git")
            .unwrap()
            .origin,
        git("git@github.com:owner/repo.git")
    );

    assert!(
        parse_request("./local/skills").unwrap().origin
            == Origin::Local(PathBuf::from("./local/skills"))
    );
    assert!(parse_request("--list ./x").unwrap().list_only);

    assert!(parse_request("").is_err());
    assert!(parse_request("not a repo").is_err());
    assert!(parse_request("owner/repo --nope").is_err());
}

#[test]
fn finds_skills_nested_in_a_source_tree() {
    let root = temp("find");
    write(&root.join("skills/alpha/SKILL.md"), &skill_md("alpha"));
    write(
        &root.join(".claude/skills/beta/SKILL.md"),
        &skill_md("beta"),
    );
    write(&root.join("README.md"), "not a skill\n");
    // Too deep to be a skill folder, and .git is never searched.
    write(&root.join("a/b/c/d/deep/SKILL.md"), &skill_md("deep"));
    write(
        &root.join(".git/objects/gamma/SKILL.md"),
        &skill_md("gamma"),
    );

    let names: Vec<String> = find_candidates(&root).into_iter().map(|c| c.name).collect();
    assert_eq!(names, ["alpha", "beta"]);
}

#[test]
fn install_copies_the_folder_and_refuses_to_clobber() {
    let root = temp("install");
    let source = root.join("src/release");
    write(&source.join("SKILL.md"), &skill_md("release"));
    write(&source.join("reference/checklist.md"), "- green CI\n");
    let dest = root.join("dest");

    let candidate = Candidate {
        name: "release".into(),
        dir: source.clone(),
    };
    let installed = install(&candidate, &dest).unwrap();
    assert_eq!(installed.path, dest.join("release"));
    assert!(dest.join("release/SKILL.md").is_file());
    assert!(dest.join("release/reference/checklist.md").is_file());

    let err = install(&candidate, &dest).expect_err("second install");
    assert!(err.contains("already installed"), "{err}");
    // Nothing left behind by the refusal.
    let leftovers: Vec<_> = std::fs::read_dir(&dest)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers, ["release"]);
}

/// A source can contain a symlink pointing anywhere on the machine;
/// copying it would smuggle that file into the skill folder, where
/// the `resource` containment check would then find it legitimately
/// inside.
#[cfg(unix)]
#[test]
fn install_skips_symlinks() {
    let root = temp("symlink");
    let source = root.join("src/sneaky");
    write(&source.join("SKILL.md"), &skill_md("sneaky"));
    write(&root.join("secret.txt"), "token\n");
    std::os::unix::fs::symlink(root.join("secret.txt"), source.join("out.txt")).unwrap();

    let dest = root.join("dest");
    install(
        &Candidate {
            name: "sneaky".into(),
            dir: source,
        },
        &dest,
    )
    .unwrap();
    assert!(dest.join("sneaky/SKILL.md").is_file());
    assert!(
        !dest.join("sneaky/out.txt").exists(),
        "the symlink must not be copied"
    );
}

#[test]
fn install_refuses_an_oversized_source() {
    let root = temp("huge");
    let source = root.join("src/heavy");
    write(&source.join("SKILL.md"), &skill_md("heavy"));
    for i in 0..(MAX_FILES + 5) {
        write(&source.join(format!("f{i}.md")), "x");
    }
    let err = install(
        &Candidate {
            name: "heavy".into(),
            dir: source,
        },
        &root.join("dest"),
    )
    .expect_err("too many files");
    assert!(err.contains("too large"), "{err}");
    assert!(!root.join("dest/heavy").exists(), "nothing half-written");
}

#[test]
fn scaffold_writes_a_loadable_template_once() {
    let root = temp("scaffold");
    let file = scaffold(&root, "release").unwrap();
    let body = std::fs::read_to_string(&file).unwrap();
    let front = crate::skills::parse_frontmatter(&body).expect("template parses");
    assert_eq!(front.name.as_deref(), Some("release"));
    assert!(front.description.is_some_and(|d| !d.is_empty()));

    let err = scaffold(&root, "release").expect_err("no clobber");
    assert!(err.contains("already exists"), "{err}");
}

#[tokio::test]
async fn checkout_of_a_local_path_is_used_in_place() {
    let root = temp("checkout");
    write(&root.join("alpha/SKILL.md"), &skill_md("alpha"));
    let origin = Origin::Local(root.clone());
    {
        let checkout = checkout(&origin).await.unwrap();
        assert_eq!(checkout.root, root);
    }
    // Dropping a local checkout must not delete the user's folder.
    assert!(root.join("alpha/SKILL.md").is_file());

    let missing = checkout(&Origin::Local(root.join("nope"))).await;
    assert!(missing.is_err());
}
