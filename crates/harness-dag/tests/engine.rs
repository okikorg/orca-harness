use orca_harness_dag::{Advance, Dag, RunState};
use serde_json::{json, Value};
fn dag(values: Value) -> Dag {
    Dag::validate(values.as_array().unwrap()).unwrap()
}
fn ids(advance: Advance) -> Vec<String> {
    match advance {
        Advance::Spawn(stages) => stages.into_iter().map(|s| s.id).collect(),
        other => panic!("expected spawns: {other:?}"),
    }
}
#[test]
fn diamond_and_duplicate_completions_emit_once() {
    let mut dag = dag(json!([
        {"id":"a","prompt":"A"}, {"id":"b","prompt":"B","needs":["a"]},
        {"id":"c","prompt":"C","needs":["a"]}, {"id":"d","prompt":"{{ stages.a.output }} {{ stages.b.output }} {{ stages.c.output }}","needs":["b","c"]}
    ]));
    assert_eq!(ids(dag.start()), ["a"]);
    assert!(ids(dag.start()).is_empty());
    assert_eq!(ids(dag.complete(&"a".into(), Ok("A".into()))), ["b", "c"]);
    assert!(ids(dag.complete(&"b".into(), Ok("B".into()))).is_empty());
    assert!(ids(dag.complete(&"b".into(), Ok("duplicate".into()))).is_empty());
    let Advance::Spawn(stages) = dag.complete(&"c".into(), Ok("C".into())) else {
        panic!()
    };
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0].prompt, "A B C");
    let Advance::Done(outcome) = dag.complete(&"d".into(), Ok("done".into())) else {
        panic!()
    };
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(outcome.outputs.len(), 1);
}
#[test]
fn rejects_bad_structure_before_start() {
    for graph in [
        json!([]),
        json!([{"id":"a","prompt":"x","needs":["missing"]}]),
        json!([{"id":"a","prompt":"x"},{"id":"a","prompt":"y"}]),
        json!([{"id":"a","prompt":"x","needs":["b"]},{"id":"b","prompt":"y","needs":["a"]}]),
        json!([{"id":"a","prompt":"{{ stages.b.output }}"},{"id":"b","prompt":"y"}]),
        json!([{"id":"a","prompt":"{{ item }}"}]),
        json!([{"id":"a","prompt":"{{ oops"}]),
        json!([{"id":"a","prompt":"x"},{"id":"m","prompt":"{{ item }}","kind":"map","over":"a"}]),
        json!([{"id":"a","prompt":"x","schema":"arbitrary"}]),
    ] {
        assert!(Dag::validate(graph.as_array().unwrap()).is_err(), "{graph}");
    }
    let err = Dag::validate(
        json!([{"id":"a","prompt":"x","needs":["b"]},{"id":"b","prompt":"x","needs":["a"]}])
            .as_array()
            .unwrap(),
    )
    .err()
    .unwrap();
    assert!(err.to_string().contains("a, b"));
}
fn mapped() -> Value {
    json!([
        {"id":"source","prompt":"list","schema":"string[]"},
        {"id":"map","prompt":"review {{ item }}","kind":"map","over":"source"},
        {"id":"report","prompt":"{{ stages.map.output }}","needs":["map"]}
    ])
}
#[test]
fn map_joins_in_input_order_retries_source_and_degrades_empty() {
    let mut d = dag(mapped());
    d.start();
    let Advance::Spawn(retry) = d.complete(&"source".into(), Ok("bad".into())) else {
        panic!()
    };
    assert!(retry[0].prompt.contains("previous answer"));
    assert_eq!(
        ids(d.complete(&"source".into(), Ok("[\"x\",\"y\"]".into()))),
        ["map[0]", "map[1]"]
    );
    assert!(ids(d.complete(&"map[1]".into(), Ok("Y".into()))).is_empty());
    let Advance::Spawn(report) = d.complete(&"map[0]".into(), Ok("X".into())) else {
        panic!()
    };
    assert_eq!(report[0].prompt, "[\"X\",\"Y\"]");
    let mut empty = dag(mapped());
    empty.start();
    assert_eq!(
        ids(empty.complete(&"source".into(), Ok("[]".into()))),
        ["report"]
    );
    let Advance::Done(outcome) = empty.complete(&"report".into(), Ok("empty".into())) else {
        panic!()
    };
    assert!(outcome.degraded);
}
#[test]
fn failures_cancellation_and_expansion_cap_are_terminal() {
    for answer in [Err("model failed".into()), Ok("bad".into())] {
        let mut d = dag(mapped());
        d.start();
        let mut advance = d.complete(&"source".into(), answer);
        if matches!(advance, Advance::Spawn(_)) {
            advance = d.complete(&"source".into(), Ok("still bad".into()));
        }
        assert!(matches!(advance, Advance::Done(ref o) if o.state == RunState::Failed));
    }
    let mut d = Dag::with_cap(mapped().as_array().unwrap(), 4).unwrap();
    d.start();
    assert!(
        matches!(d.complete(&"source".into(),Ok("[\"a\",\"b\"]".into())),Advance::Done(ref o) if o.error.as_ref().unwrap().contains("cap"))
    );
    let mut d = dag(mapped());
    d.start();
    assert!(matches!(d.cancel(),Advance::Done(ref o) if o.state == RunState::Cancelled));
    assert!(ids(d.complete(&"source".into(), Ok("[]".into()))).is_empty());
    assert!(ids(d.cancel()).is_empty());
}
#[test]
fn replay_keys_change_only_with_ancestors_and_are_order_independent() {
    let input = json!([{"id":"a","prompt":"A"},{"id":"b","prompt":"B","needs":["a"]},{"id":"c","prompt":"C"}]);
    let original = dag(input.clone());
    let mut changed = input.clone();
    changed[0]["prompt"] = json!("new");
    let changed = dag(changed);
    assert_ne!(
        original.stage_key(&"a".into()),
        changed.stage_key(&"a".into())
    );
    assert_ne!(
        original.stage_key(&"b".into()),
        changed.stage_key(&"b".into())
    );
    assert_eq!(
        original.stage_key(&"c".into()),
        changed.stage_key(&"c".into())
    );
    let mut reordered = input.as_array().unwrap().clone();
    reordered.reverse();
    assert_eq!(
        original.stage_key(&"b".into()),
        Dag::validate(&reordered).unwrap().stage_key(&"b".into())
    );
}
#[test]
fn maps_can_feed_maps() {
    let mut graph = mapped();
    graph
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"map2","kind":"map","over":"map","prompt":"verify {{ item }}"}));
    let mut d = dag(graph);
    d.start();
    d.complete(&"source".into(), Ok("[\"a\"]".into()));
    let emitted = ids(d.complete(&"map[0]".into(), Ok("answer".into())));
    assert_eq!(emitted, ["map2[0]", "report"]);
}
