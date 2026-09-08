//! What recording and resuming a session costs.
//!
//! `SessionHandler` is subscribed to `before_model` and `on_agent_end`, so
//! `sync` runs between every model call — it is the one blocking write on
//! the agent's inner loop. It is append-only in the common case, and a
//! full rewrite whenever the context got *shorter* than what is on disk,
//! which is exactly what compaction does. Those two are different
//! complexities on the same method, so they are benchmarked apart.
//!
//! The read side runs at startup: `SessionFile::list` builds the session
//! picker by opening every file in the directory for its header, and
//! `load` parses one transcript back into a `Context`. Both are felt as
//! launch lag, not as loop overhead, so they are measured against the
//! sizes a long-lived workspace accumulates rather than a fresh one.
//!
//! `context_window.rs` measures the in-memory side of the same story;
//! this is the disk that backs it. Both write to a tempdir, so the numbers
//! carry the host filesystem's `fsync` behaviour — treat them as a
//! same-machine regression signal, not an absolute.

use std::fs;
use std::path::PathBuf;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use serde_json::json;

use orca_harness_core::{Context, ToolCall, ToolResult};
use orca_harness_extensions::{SessionFile, SessionHandler};

/// A tool result big enough to matter: transcripts are dominated by tool
/// output, not by prose, and the serializer's cost tracks that.
const RESULT_BYTES: usize = 2 * 1024;

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "orca-session-bench-{}-{tag}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `turns` rounds of user / assistant-with-tool-call / tool-result — the
/// three messages a real turn appends.
fn transcript(turns: usize) -> Context {
    let mut context = Context::new();
    context.push_system("You are a coding agent operating in a git repository.");
    for i in 0..turns {
        context.push_user(format!("Turn {i}: investigate the regression."));
        context.push_assistant_tool_calls(
            Some(format!("Reading the module for turn {i}.")),
            vec![ToolCall {
                id: format!("call-{i}"),
                name: "read_file".into(),
                arguments: json!({"path": format!("crates/core/src/mod_{i}.rs")}),
            }],
        );
        context.append_tool_results(vec![ToolResult {
            call_id: format!("call-{i}"),
            tool_name: "read_file".into(),
            output: json!({"content": "y".repeat(RESULT_BYTES)}),
            is_error: false,
        }]);
    }
    context
}

/// Steady state: one turn's three messages appended to an already-long
/// session. This is the per-model-call write, and the number that must not
/// grow with session length.
fn append(c: &mut Criterion) {
    let mut group = c.benchmark_group("session/append");
    for turns in [10usize, 200, 1_000] {
        group.bench_with_input(
            BenchmarkId::new("existing_turns", turns),
            &turns,
            |b, turns| {
                let dir = Dir::new("append");
                let handler = SessionHandler::create(dir.0.clone(), "/workspace", "model").unwrap();
                let mut context = transcript(*turns);
                handler.sync(&context);
                let mut i = *turns;
                b.iter(|| {
                    i += 1;
                    context.push_user(format!("Turn {i}: investigate the regression."));
                    context.push_assistant_text(format!("Answer {i}."));
                    handler.sync(black_box(&context));
                });
            },
        );
    }
    group.finish();
}

/// The rewrite branch: a context shorter than what is on disk, which is
/// what the host hands `sync` immediately after a compaction. Cost is the
/// whole new transcript, not the delta.
fn rewrite_after_compaction(c: &mut Criterion) {
    let mut group = c.benchmark_group("session/rewrite");
    for turns in [10usize, 200, 1_000] {
        let long = transcript(turns);
        // What compaction leaves behind: a summary plus a short tail.
        let compacted = transcript(3);
        group.bench_with_input(BenchmarkId::new("from_turns", turns), &long, |b, long| {
            b.iter_batched(
                || {
                    let dir = Dir::new("rewrite");
                    let handler =
                        SessionHandler::create(dir.0.clone(), "/workspace", "model").unwrap();
                    handler.sync(long);
                    (dir, handler)
                },
                |(dir, handler)| {
                    handler.sync(black_box(&compacted));
                    dir
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

/// Resume: parse one transcript off disk back into a `Context`. Runs once
/// per `--resume`, `/sessions` switch, and fork.
fn load(c: &mut Criterion) {
    let mut group = c.benchmark_group("session/load");
    for turns in [10usize, 200, 1_000] {
        let dir = Dir::new("load");
        let handler = SessionHandler::create(dir.0.clone(), "/workspace", "model").unwrap();
        handler.sync(&transcript(turns));
        let path = handler.path();
        group.throughput(criterion::Throughput::Bytes(
            fs::metadata(&path).unwrap().len(),
        ));
        group.bench_with_input(BenchmarkId::new("turns", turns), &path, |b, path| {
            b.iter(|| black_box(SessionFile::load(path).unwrap()))
        });
        drop(dir);
    }
    group.finish();
}

/// The session picker: one open and one header read per file in the
/// directory. Nothing prunes it, so a workspace's session count only ever
/// grows, and this is on the startup path of `/sessions`.
fn list(c: &mut Criterion) {
    let mut group = c.benchmark_group("session/list");
    for count in [10usize, 100, 500] {
        let dir = Dir::new("list");
        for _ in 0..count {
            let handler = SessionHandler::create(dir.0.clone(), "/workspace", "model").unwrap();
            handler.sync(&transcript(5));
        }
        group.throughput(criterion::Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("sessions", count), &dir.0, |b, path| {
            b.iter(|| black_box(SessionFile::list(path).len()))
        });
        drop(dir);
    }
    group.finish();
}

criterion_group!(benches, append, rewrite_after_compaction, load, list);
criterion_main!(benches);
