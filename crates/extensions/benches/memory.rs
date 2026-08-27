use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use rusqlite::{params, Connection};

use orca_harness_core::{CancellationToken, Context, Tool, ToolContext};
use orca_harness_extensions::{
    MemoryExtension, MemoryManageTool, MemoryScope, MemoryStore, MEMORY_MANAGE_TOOL,
};
use serde_json::json;

const RECORDS: usize = 50_000;
const GLOBAL_RECORDS: usize = 5_000;
const RAW_QUERY: &str = "
    SELECT m.public_id, m.content, m.kind, m.is_global, m.workspace_id,
           m.workspace_root, m.source_call_id, m.created_at, m.updated_at
    FROM memories_fts
    JOIN memories AS m ON m.id = memories_fts.rowid
    WHERE memories_fts MATCH ?1
      AND (m.is_global = 1 OR m.workspace_id = ?2)
    ORDER BY memories_fts.rank, m.updated_at DESC
    LIMIT ?3
";

struct Fixture {
    dir: PathBuf,
    store: MemoryStore,
    scope: MemoryScope,
    raw: Connection,
}

impl Fixture {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("orcacode-memory-bench-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("memory.sqlite3");
        let store = MemoryStore::open(&path).expect("open benchmark database");
        let scope = MemoryScope::new("benchmark-workspace", "/benchmark/workspace");
        for index in 0..RECORDS {
            let is_global = index < GLOBAL_RECORDS;
            let content = format!(
                "For entity_{index:05}, use cargo check before the focused Rust test; preserve unrelated workspace changes"
            );
            store
                .save(&scope, &content, "workflow", is_global, "benchmark-seed")
                .expect("seed benchmark memory");
        }
        let raw = Connection::open(&path).expect("open raw benchmark connection");
        raw.busy_timeout(Duration::from_secs(2))
            .expect("configure raw busy timeout");
        raw.pragma_update(None, "synchronous", "NORMAL")
            .expect("configure raw synchronous mode");
        Self {
            dir,
            store,
            scope,
            raw,
        }
    }

    fn raw_search(&self, query: &str, limit: usize) -> usize {
        let mut statement = self.raw.prepare(RAW_QUERY).expect("prepare raw query");
        statement
            .query_map(
                params![query, self.scope.workspace_id, limit as i64],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, bool>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                    ))
                },
            )
            .expect("execute raw query")
            .collect::<Result<Vec<_>, _>>()
            .expect("map raw rows")
            .len()
    }

    fn raw_forget(&self, id: &str) -> bool {
        self.raw
            .execute(
                "DELETE FROM memories
                 WHERE public_id = ?1 AND (is_global = 1 OR workspace_id = ?2)",
                params![id, self.scope.workspace_id],
            )
            .expect("execute raw forget")
            == 1
    }

    fn record_for_forget(&self, sequence: usize) -> String {
        self.store
            .save(
                &self.scope,
                &format!("forget_probe_{sequence}"),
                "fact",
                false,
                "benchmark-forget",
            )
            .expect("seed forget benchmark")
            .id
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn bench_memory(c: &mut Criterion) {
    let fixture = Fixture::new();
    let extension = MemoryExtension::new(fixture.store.clone(), fixture.scope.clone());
    let mut group = c.benchmark_group("memory_fts_50k");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(3));

    for (name, plain, raw) in [
        ("exact", "entity_42420", "\"entity_42420\""),
        ("broad", "cargo check", "\"cargo\" OR \"check\""),
    ] {
        group.bench_function(format!("raw_sql/{name}"), |b| {
            b.iter(|| fixture.raw_search(black_box(raw), black_box(8)))
        });
        group.bench_function(format!("orcacode_store/{name}"), |b| {
            b.iter(|| {
                fixture
                    .store
                    .search(&fixture.scope, black_box(plain), black_box(8))
                    .expect("OrcaCode store query")
            })
        });
        group.bench_function(format!("orcacode_context/{name}"), |b| {
            b.iter_batched(
                || {
                    let mut context = Context::new();
                    context.push_user(plain);
                    context
                },
                |context| {
                    extension
                        .prepare_context(black_box(&context))
                        .expect("OrcaCode context query")
                },
                BatchSize::SmallInput,
            )
        });
    }

    let sequence = Cell::new(0);
    group.bench_function("raw_sql/forget", |b| {
        b.iter_batched(
            || {
                let next = sequence.get() + 1;
                sequence.set(next);
                fixture.record_for_forget(next)
            },
            |id| black_box(fixture.raw_forget(black_box(&id))),
            BatchSize::SmallInput,
        )
    });

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build benchmark runtime");
    let tool = Arc::new(MemoryManageTool::new(
        fixture.store.clone(),
        fixture.scope.clone(),
    ));
    group.bench_function("memory_manage/forget_no_approval", |b| {
        b.to_async(&runtime).iter_batched(
            || {
                let next = sequence.get() + 1;
                sequence.set(next);
                let id = fixture.record_for_forget(next);
                let input = json!({"action": "forget", "id": id});
                let context = ToolContext {
                    call_id: format!("forget-{next}"),
                    tool_name: MEMORY_MANAGE_TOOL.into(),
                    cancellation: CancellationToken::new(),
                    deadline: None,
                };
                (input, context)
            },
            |(input, context)| {
                let tool = tool.clone();
                async move {
                    black_box(
                        tool.call(black_box(input), black_box(&context))
                            .await
                            .expect("memory_manage forget"),
                    )
                }
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

criterion_group!(benches, bench_memory);
criterion_main!(benches);
