//! What skills cost. Three numbers matter, and they sit on different
//! paths:
//!
//! - **Discovery** runs at startup and again on every `/skills` toggle,
//!   `create`, `remove`, and `reload` — with a rebuild of the agent
//!   behind it. It is the one that would be felt as lag.
//! - **Schema** (the catalog the model sees) is rebuilt with the agent,
//!   so it rides on the same keypress as discovery.
//! - **Load** is the `skill` tool call itself: one file read on the
//!   agent's critical path, between the model asking and the model
//!   continuing.
//!
//! Unloading is not a separate cost: turning a skill off is a config
//! write plus the same discovery pass, and deleting one is `remove_dir_all`
//! plus that pass. Both are benchmarked here as `toggle` and `delete`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use serde_json::json;

use orca_harness_core::{CancellationToken, Tool, ToolContext};
use orca_harness_tool_extensions::skills::{
    discover, roots, uninstall, Skill, SkillRoot, SkillTool,
};

/// A body big enough to be realistic: published skills run a few
/// kilobytes, and the one measured here is on the larger side.
const BODY: usize = 4 * 1024;

struct Tree(PathBuf);

impl Tree {
    /// A workspace with `count` skills spread over the two roots a
    /// project uses, plus the noise a real tree carries: folders with no
    /// `SKILL.md`, and roots that do not exist at all.
    fn new(tag: &str, count: usize) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "orca-skills-bench-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        for index in 0..count {
            let root = if index % 2 == 0 {
                ".orca/skills"
            } else {
                ".claude/skills"
            };
            let path = dir
                .join(root)
                .join(format!("skill-{index:03}"))
                .join("SKILL.md");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, skill_md(&format!("skill-{index:03}"))).unwrap();
            // Half the skills carry a resource file beside them.
            if index % 2 == 0 {
                fs::write(path.parent().unwrap().join("reference.md"), "x".repeat(512)).unwrap();
            }
        }
        // Directories that are not skills still get walked past.
        for index in 0..count / 4 {
            let path = dir.join(".orca/skills").join(format!("notes-{index}"));
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("README.md"), "not a skill").unwrap();
        }
        Self(dir)
    }

    fn roots(&self) -> Vec<SkillRoot> {
        // The real shape: eleven roots, most of which do not exist.
        roots(
            &self.0,
            Some(&self.0.join("config")),
            Some(&self.0.join("home")),
        )
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn skill_md(name: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: One line about {name}, long enough to look like a real \
         catalog entry rather than a placeholder.\n---\n\n{}\n",
        "step. ".repeat(BODY / 6)
    )
}

fn ctx() -> ToolContext {
    ToolContext {
        call_id: "1".into(),
        tool_name: "skill".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

/// Startup, and every toggle: scan the roots and parse the frontmatter.
fn bench_discovery(c: &mut Criterion) {
    let mut group = c.benchmark_group("skills/discover");
    for count in [1usize, 10, 25, 100] {
        let tree = Tree::new("discover", count);
        let roots = tree.roots();
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                let found = discover(std::hint::black_box(&roots));
                assert_eq!(found.skills.len(), count);
                found
            })
        });
    }
    group.finish();
}

/// Rebuilt with the agent on every toggle: the catalog the model reads.
fn bench_schema(c: &mut Criterion) {
    let mut group = c.benchmark_group("skills/schema");
    for count in [1usize, 25, 100] {
        let tree = Tree::new("schema", count);
        let skills: Vec<Skill> = discover(&tree.roots()).skills;
        let tool = SkillTool::new(skills);
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| std::hint::black_box(tool.schema()))
        });
    }
    group.finish();
}

/// The agent's critical path: one call, one file read, one 8k page.
fn bench_load(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let tree = Tree::new("load", 25);
    let tool = SkillTool::new(discover(&tree.roots()).skills);
    let ctx = ctx();

    let mut group = c.benchmark_group("skills/load");
    group.bench_function("instructions", |b| {
        b.to_async(&runtime)
            .iter(|| async { tool.call(json!({"name": "skill-000"}), &ctx).await.unwrap() })
    });
    group.bench_function("resource", |b| {
        b.to_async(&runtime).iter(|| async {
            tool.call(
                json!({"name": "skill-000", "resource": "reference.md"}),
                &ctx,
            )
            .await
            .unwrap()
        })
    });
    group.finish();
}

/// Unloading. A toggle is a scan the tool set is rebuilt from; a delete
/// is the folder going away and the same scan afterwards.
fn bench_unload(c: &mut Criterion) {
    let mut group = c.benchmark_group("skills/unload");
    let tree = Tree::new("toggle", 25);
    let roots = tree.roots();
    group.bench_function("toggle", |b| {
        // What the worker does after the config write: rescan, then
        // rebuild the tool over what is left enabled.
        b.iter(|| {
            let found = discover(std::hint::black_box(&roots));
            let kept: Vec<Skill> = found
                .skills
                .into_iter()
                .filter(|skill| skill.name != "skill-000")
                .collect();
            std::hint::black_box(SkillTool::new(kept).schema())
        })
    });
    group.bench_function("delete", |b| {
        b.iter_batched(
            || {
                let tree = Tree::new("delete", 25);
                let dir = tree.0.join(".orca/skills/skill-000");
                (tree, dir)
            },
            |(tree, dir)| {
                uninstall(&dir).unwrap();
                let found = discover(&tree.roots());
                assert_eq!(found.skills.len(), 24);
                found
            },
            criterion::BatchSize::PerIteration,
        )
    });
    group.finish();
}

/// A sanity check that the numbers above are about the right tree: a
/// realistic install is tens of skills, not thousands.
fn bench_cold_paths(c: &mut Criterion) {
    let tree = Tree::new("missing", 0);
    let roots = tree.roots();
    c.bench_function("skills/discover/empty", |b| {
        b.iter(|| std::hint::black_box(discover(&roots)))
    });
    let path: &Path = &tree.0;
    assert!(!path.join("home/.claude/skills").exists());
}

criterion_group!(
    benches,
    bench_discovery,
    bench_schema,
    bench_load,
    bench_unload,
    bench_cold_paths
);
criterion_main!(benches);
