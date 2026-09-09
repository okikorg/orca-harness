//! What a workflow graph costs the host, in the two places the cost is
//! actually paid.
//!
//! - **Admission** (`Dag::with_cap`) runs once, synchronously, inside the
//!   `workflow` tool call — between the model submitting a graph and the
//!   first stage being dispatched. It parses, validates, and computes the
//!   transitive ancestor set of every stage to check template references.
//! - **Scheduling** (`start` / `complete`) runs on the critical path of
//!   *every* stage: a stage's result comes back, the engine decides what
//!   is newly ready, renders its prompts, and hands them out. Nothing else
//!   happens between one stage's model returning and the next one's
//!   starting, so this is pure harness overhead.
//!
//! The e2e suite under `benchmarks/workflow/` measures dispatch delay
//! through the real `WorkflowTool` with a fixed-delay model; its reference
//! run reports chain dispatch growing linearly with chain length. This
//! bench is the unit underneath that observation: it drives the same
//! engine with no executor, no model, and no timer, so the curve is the
//! engine's and nothing else's. Admission uses compact ancestor bits; scheduling uses an ordered ready
//! queue and counts map completions. These shapes track their scaling and
//! guard against reintroducing full pending-stage or child-output rescans.
//!
//! Sizes stop at `DEFAULT_STAGE_CAP` (256) because that is the largest
//! graph the shipped tool will admit — a curve past it would measure code
//! no user can reach. Map expansion counts children against the same cap,
//! so its sizes are smaller by construction.

use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use serde_json::{json, Value};

use orca_harness_dag::{Advance, Dag, RunState, StageId, DEFAULT_STAGE_CAP};

/// Prompt bodies are the one part of a stage whose size is the model's
/// choice rather than the harness's. A few hundred bytes is a realistic
/// instruction and keeps the clone/render cost visible without letting
/// `memcpy` dominate the scheduling measurement.
const PROMPT: &str = "Take the upstream result and produce the next artefact. \
Be specific, cite the file you changed, and return only the artefact itself \
with no preamble, no restatement of this instruction, and no trailing notes.";

/// `s0 -> s1 -> ... -> s(n-1)`, each stage templating its predecessor.
/// Every stage inherits the whole prefix, and each completion releases
/// exactly one dependent. This separates admission from scheduling costs.
fn chain(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| {
            if i == 0 {
                json!({"id": format!("s{i}"), "prompt": PROMPT})
            } else {
                let prior = format!("s{}", i - 1);
                json!({
                    "id": format!("s{i}"),
                    "prompt": format!("{PROMPT} Prior: {{{{ stages.{prior}.output }}}}"),
                    "needs": [prior],
                })
            }
        })
        .collect()
}

/// One root, `n - 2` independent workers, one join over all of them. The
/// best case for the scheduler — a single `emit` releases the whole middle
/// layer — and the worst for the join's ancestor set.
fn fanout_join(n: usize) -> Vec<Value> {
    let workers = n.saturating_sub(2);
    let mut stages = vec![json!({"id": "root", "prompt": PROMPT})];
    stages.extend((0..workers).map(|i| {
        json!({
            "id": format!("w{i}"),
            "prompt": format!("{PROMPT} Prior: {{{{ stages.root.output }}}}"),
            "needs": ["root"],
        })
    }));
    stages.push(json!({
        "id": "join",
        "prompt": PROMPT,
        "needs": (0..workers).map(|i| format!("w{i}")).collect::<Vec<_>>(),
    }));
    stages
}

/// A root emitting `items` values, a map over them, and a join on the map.
/// Expansion happens inside one `complete`, so this isolates the cost of
/// materialising children — clone, render, four map inserts each — from
/// the steady-state scheduling around it.
fn map_graph() -> Vec<Value> {
    vec![
        json!({"id": "src", "prompt": PROMPT, "schema": "string[]"}),
        json!({
            "id": "work",
            "prompt": format!("{PROMPT} Item: {{{{ item }}}}"),
            "kind": "map",
            "over": "src",
        }),
        json!({
            "id": "join",
            "prompt": format!("{PROMPT} Prior: {{{{ stages.work.output }}}}"),
            "needs": ["work"],
        }),
    ]
}

/// A `string[]` answer the map source can expand into `items` children.
fn items(count: usize) -> String {
    serde_json::to_string(&(0..count).map(|i| format!("item-{i}")).collect::<Vec<_>>()).unwrap()
}

/// Run a validated graph to completion, answering every stage the moment
/// it is spawned. The `Dag` is handed back rather than dropped so its
/// teardown lands outside `iter_batched`'s timer. This is the whole scheduling cost of a workflow with the
/// model's time removed: `n` completions, each releasing newly ready work.
///
/// Answers are the map-source payload throughout — a plain JSON array —
/// so a schema'd stage parses on the first try and no shape pays a retry
/// the others do not.
fn drive(mut dag: Dag, answer: &str) -> (Dag, usize) {
    let mut stages = 0usize;
    let mut queue: Vec<StageId> = match dag.start() {
        Advance::Spawn(ready) => ready.into_iter().map(|s| s.id).collect(),
        other => panic!("graph did not start: {other:?}"),
    };
    while let Some(id) = queue.pop() {
        stages += 1;
        match dag.complete(&id, Ok(answer.to_string())) {
            Advance::Spawn(ready) => queue.extend(ready.into_iter().map(|s| s.id)),
            Advance::Done(outcome) => {
                assert_eq!(outcome.state, RunState::Done, "{:?}", outcome.error);
                assert!(queue.is_empty(), "finished with {} in flight", queue.len());
            }
            Advance::Stalled(why) => panic!("stalled: {why}"),
        }
    }
    (dag, stages)
}

/// Admission: parse + validate + ancestor closure, once per submitted
/// graph, on the model's critical path inside the `workflow` tool.
fn admission(c: &mut Criterion) {
    let mut group = c.benchmark_group("dag/admit");
    for n in [8usize, 32, 128, 256] {
        let chain = chain(n);
        let fanout = fanout_join(n);
        group.bench_with_input(BenchmarkId::new("chain", n), &chain, |b, stages| {
            b.iter(|| Dag::with_cap(black_box(stages), DEFAULT_STAGE_CAP).unwrap())
        });
        group.bench_with_input(BenchmarkId::new("fanout_join", n), &fanout, |b, stages| {
            b.iter(|| Dag::with_cap(black_box(stages), DEFAULT_STAGE_CAP).unwrap())
        });
    }
    group.finish();
}

/// Scheduling: `n` completions driven to `Done`. Admission is done in
/// `iter_batched`'s setup and left untimed — it is a once-per-graph cost
/// already measured above, and in a 256-stage chain it is larger than the
/// scheduling it would otherwise hide. Divide by `n` for the per-stage
/// overhead a workflow pays between one model returning and the next
/// being entered.
fn scheduling(c: &mut Criterion) {
    let answer = items(4);
    let mut group = c.benchmark_group("dag/schedule");
    for n in [8usize, 32, 128, 256] {
        group.throughput(criterion::Throughput::Elements(n as u64));
        let chain = chain(n);
        let fanout = fanout_join(n);
        for (name, stages) in [("chain", &chain), ("fanout_join", &fanout)] {
            group.bench_with_input(BenchmarkId::new(name, n), stages, |b, stages| {
                b.iter_batched(
                    || Dag::with_cap(stages, DEFAULT_STAGE_CAP).unwrap(),
                    |dag| drive(dag, &answer),
                    BatchSize::SmallInput,
                )
            });
        }
    }
    group.finish();
}

/// Map expansion plus the join barrier over its children. The graph is
/// three stages; the work is the `items` children created inside one
/// `complete` and the ordered join that settles them.
fn map_expansion(c: &mut Criterion) {
    let graph = map_graph();
    let mut group = c.benchmark_group("dag/map");
    for count in [4usize, 16, 64, 200] {
        let answer = items(count);
        group.throughput(criterion::Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("expand", count), &answer, |b, answer| {
            b.iter_batched(
                || Dag::with_cap(&graph, DEFAULT_STAGE_CAP).unwrap(),
                |dag| drive(dag, answer),
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

/// `stage_key` is the host's cache key for a stage: a recursive walk of
/// its ancestry, serialized. It is called once per stage before dispatch,
/// and in a chain the deepest stage's ancestry is the entire graph.
fn stage_keys(c: &mut Criterion) {
    let mut group = c.benchmark_group("dag/stage_key");
    for n in [8usize, 32, 128, 256] {
        let dag = Dag::with_cap(&chain(n), DEFAULT_STAGE_CAP).unwrap();
        let ids: Vec<StageId> = dag.stages().map(|s| s.id.clone()).collect();
        group.throughput(criterion::Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("chain_all", n), &ids, |b, ids| {
            b.iter(|| {
                for id in ids {
                    black_box(dag.stage_key(id));
                }
            })
        });
    }
    group.finish();
}

criterion_group!(benches, admission, scheduling, map_expansion, stage_keys);
criterion_main!(benches);
