//! What the tools an agent actually calls cost.
//!
//! `read_file`, `edit_file`, `grep`, and `glob` are the four an agent
//! spends most of its calls on, and every one of them sits directly on the
//! critical path: the model stops until the result comes back. Their cost
//! is not the syscall — it is the work the tool does around it, and that
//! work is superlinear in places the schema does not hint at:
//!
//! - `edit_file` scans the whole file for occurrences of `old` before
//!   replacing anything, because a non-unique match is an error rather
//!   than a guess. That is a full pass per edit, plus a full rewrite.
//! - `grep` and `glob` walk the tree themselves rather than shelling out
//!   to ripgrep — a deliberate dependency choice — so tree size, not
//!   match count, sets the price. Both cap their results, and the cap does
//!   not stop the walk.
//! - `glob`'s matcher is a hand-rolled dynamic program per path segment,
//!   run against every entry it visits.
//!
//! Fixtures are a synthetic source tree, sized around what these tools are
//! pointed at in practice: this workspace is ~68k lines across a few
//! hundred files, and an agent working in a monorepo sees an order of
//! magnitude more. Files are ordinary text on the host filesystem, so
//! absolute numbers carry the page cache with them — read these as a
//! same-machine regression signal.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use serde_json::json;

use orca_harness_core::{CancellationToken, Tool, ToolContext};
use orca_harness_tools::{EditFileTool, GlobTool, GrepTool, ReadFileTool, Workspace};

/// A line of plausible Rust. Long enough that byte counts track file
/// counts realistically, short enough that a file of a few hundred lines
/// is a few tens of kilobytes, as real source is.
const LINE: &str =
    "    let outcome = registry.dispatch(call, &context).await.map_err(Error::from)?;";

struct Tree(PathBuf);

impl Tree {
    /// `files` source files spread over `files / 8` module directories,
    /// each `lines` long. One file per tree carries a unique marker so a
    /// grep can be made to match exactly once instead of everywhere.
    fn new(tag: &str, files: usize, lines: usize) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "orca-core-tools-bench-{}-{tag}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let body = format!("{LINE}\n").repeat(lines);
        for i in 0..files {
            let dir = root.join(format!("crates/pkg_{}/src", i / 8));
            fs::create_dir_all(&dir).unwrap();
            let mut text = format!("//! module {i}\n{body}");
            if i == files / 2 {
                text.push_str("// UNIQUE-MARKER-FOR-THIS-TREE\n");
            }
            fs::write(dir.join(format!("module_{i}.rs")), text).unwrap();
        }
        Self(root)
    }

    fn ws(&self) -> Workspace {
        Workspace::new(&self.0)
    }

    /// A path into the tree, relative to the workspace root.
    fn rel(&self, index: usize) -> String {
        format!("crates/pkg_{}/src/module_{index}.rs", index / 8)
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn ctx(name: &str) -> ToolContext {
    ToolContext {
        call_id: format!("{name}-0"),
        tool_name: name.into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

/// `read_file` against file size. Nearly all of it is the read and the
/// UTF-8 check; the point of the curve is that nothing else grows.
fn read(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("tools/read_file");
    for lines in [100usize, 2_000, 20_000] {
        let tree = Tree::new("read", 1, lines);
        let tool = ReadFileTool::new(tree.ws());
        let input = json!({ "path": tree.rel(0) });
        let bytes = fs::metadata(tree.0.join(tree.rel(0))).unwrap().len();
        group.throughput(criterion::Throughput::Bytes(bytes));
        group.bench_with_input(BenchmarkId::new("lines", lines), &input, |b, input| {
            b.to_async(&rt).iter(|| async {
                black_box(tool.call(input.clone(), &ctx("read_file")).await.unwrap())
            })
        });
    }
    group.finish();
}

/// `edit_file` against file size. Each call reads the file, counts every
/// occurrence of `old`, rewrites, and writes it back — several passes over
/// the file for an edit of a few bytes. The tree is rebuilt per batch
/// because an edit is destructive: a second call would not find its `old`
/// text. The fixture is built in the untimed setup and handed back out of
/// the routine so `iter_batched` tears it down after the timer stops —
/// `remove_dir_all` over a 1.6 MB file is larger than the edit itself.
///
/// What remains is dominated by the write-back, not by the scan: at 20k
/// lines this costs tens of times a `read_file` of the same file, and its
/// spread is the host filesystem's rather than the tool's.
fn edit(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("tools/edit_file");
    for lines in [100usize, 2_000, 20_000] {
        group.bench_with_input(BenchmarkId::new("lines", lines), &lines, |b, lines| {
            b.to_async(&rt).iter_batched(
                || {
                    let tree = Tree::new("edit", 1, *lines);
                    let tool = EditFileTool::new(tree.ws());
                    let input = json!({
                        "path": tree.rel(0),
                        "old": "//! module 0",
                        "new": "//! module 0 (edited)",
                    });
                    (tree, tool, input)
                },
                |(tree, tool, input)| async move {
                    tool.call(input, &ctx("edit_file")).await.unwrap();
                    tree
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

/// `grep` against tree size, at both ends of the result cap. A query that
/// hits every line saturates the 200-result cap early; one that hits once
/// walks the entire tree. The gap between them is what the cap actually
/// buys, and the second number is what a real "where is this symbol"
/// search pays.
fn grep(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("tools/grep");
    group.sample_size(20);
    for files in [50usize, 400, 2_000] {
        let tree = Tree::new("grep", files, 200);
        let tool = GrepTool::new(tree.ws());
        group.throughput(criterion::Throughput::Elements(files as u64));
        for (name, query) in [
            ("saturating", "registry.dispatch"),
            ("single_match", "UNIQUE-MARKER-FOR-THIS-TREE"),
        ] {
            let input = json!({ "query": query, "path": "." });
            group.bench_with_input(BenchmarkId::new(name, files), &input, |b, input| {
                b.to_async(&rt).iter(|| async {
                    black_box(tool.call(input.clone(), &ctx("grep")).await.unwrap())
                })
            });
        }
    }
    group.finish();
}

/// `glob` against tree size. The walk is the same for every pattern; what
/// changes is how much of it the matcher runs on. An anchored `**` pattern
/// runs the segment dynamic program over every path component, a bare
/// `*.rs` only over the file name.
fn glob(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("tools/glob");
    group.sample_size(20);
    for files in [50usize, 400, 2_000] {
        let tree = Tree::new("glob", files, 20);
        let tool = GlobTool::new(tree.ws());
        group.throughput(criterion::Throughput::Elements(files as u64));
        for (name, pattern) in [
            ("name_only", "*.rs"),
            ("anchored_deep", "crates/**/src/module_*.rs"),
        ] {
            let input = json!({ "pattern": pattern });
            group.bench_with_input(BenchmarkId::new(name, files), &input, |b, input| {
                b.to_async(&rt).iter(|| async {
                    black_box(tool.call(input.clone(), &ctx("glob")).await.unwrap())
                })
            });
        }
    }
    group.finish();
}

criterion_group!(benches, read, edit, grep, glob);
criterion_main!(benches);
