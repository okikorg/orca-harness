use orca_harness_dag::{Advance, Dag, RunState, StageStatus};
use serde_json::json;

fn spawned(advance: Advance) -> Vec<orca_harness_dag::Stage> {
    match advance {
        Advance::Spawn(stages) => stages,
        other => panic!("expected spawn: {other:?}"),
    }
}

#[test]
fn empty_maps_preserve_ready_wave_order() {
    let mut dag = Dag::validate(&[
        json!({"id":"source","prompt":"list","schema":"string[]"}),
        json!({"id":"b_map","prompt":"{{ item }}","kind":"map","over":"source"}),
        json!({"id":"a_next","prompt":"next","needs":["b_map"]}),
        json!({"id":"z_worker","prompt":"work","needs":["source"]}),
    ])
    .unwrap();
    dag.start();
    let stages = spawned(dag.complete(&"source".into(), Ok("[]".into())));
    assert_eq!(
        stages.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["z_worker", "a_next"]
    );
    assert_eq!(dag.output(&"b_map".into()), Some("[]"));
}

#[test]
fn map_retry_duplicate_completion_provenance_and_terminal_contracts() {
    let mut dag = Dag::validate(&[
        json!({"id":"source","prompt":"list","schema":"json[]"}),
        json!({"id":"map","prompt":"{{ stages.source.output }} / {{ item }}","kind":"map","over":"source","schema":"string[]","model":"chosen"}),
        json!({"id":"next","prompt":"{{ item }}","kind":"map","over":"map"}),
    ]).unwrap();
    dag.start();
    let children = spawned(dag.complete(&"source".into(), Ok("[1,2]".into())));
    assert_eq!(children[0].prompt, "[1,2] / 1");
    assert_eq!(children[0].model.as_deref(), Some("chosen"));
    assert_eq!(dag.map_source(&children[0].id), Some(&"source".into()));
    let key = dag.stage_key(&children[0].id);
    let retry = spawned(dag.complete(&children[0].id, Ok("bad".into())));
    assert!(retry[0]
        .prompt
        .starts_with("[1,2] / 1\nYour previous answer"));
    assert_eq!(dag.stage_key(&children[0].id), key);
    assert!(spawned(dag.complete(&children[1].id, Ok("[\"two\"]".into()))).is_empty());
    assert!(spawned(dag.complete(&children[1].id, Ok("duplicate".into()))).is_empty());
    let next = spawned(dag.complete(&children[0].id, Ok("[\"one\"]".into())));
    assert_eq!(next.len(), 2);
    assert_eq!(dag.map_source(&next[0].id), Some(&children[0].id));
    assert_eq!(dag.map_source(&next[1].id), Some(&children[1].id));
    assert_eq!(next[0].prompt, "[\"one\"]");
    dag.complete(&next[1].id, Ok("second".into()));
    let Advance::Done(outcome) = dag.complete(&next[0].id, Ok("first".into())) else {
        panic!()
    };
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(outcome.outputs.len(), 1);
    assert_eq!(outcome.outputs["next"], "[\"first\",\"second\"]");
    assert!(outcome.stages.values().all(|s| *s == StageStatus::Done));
    assert_eq!(dag.output(&children[0].id), Some("[\"one\"]"));
}

#[test]
fn failure_stops_siblings_and_cancel_marks_all_unfinished() {
    for cancel in [false, true] {
        let mut dag = Dag::validate(&[
            json!({"id":"a","prompt":"A"}),
            json!({"id":"b","prompt":"B"}),
            json!({"id":"c","prompt":"C","needs":["b"]}),
        ])
        .unwrap();
        dag.start();
        let result = if cancel {
            dag.cancel()
        } else {
            dag.complete(&"a".into(), Err("oops".into()))
        };
        let Advance::Done(outcome) = result else {
            panic!()
        };
        assert_eq!(
            outcome.stages["a"],
            if cancel {
                StageStatus::Cancelled
            } else {
                StageStatus::Failed
            }
        );
        for id in ["b", "c"] {
            assert_eq!(
                outcome.stages[id],
                if cancel {
                    StageStatus::Cancelled
                } else {
                    StageStatus::Stopped
                }
            );
        }
        assert!(spawned(dag.complete(&"b".into(), Ok("late".into()))).is_empty());
    }
}

#[test]
fn transitive_templates_cross_word_boundaries_and_error_order_is_stable() {
    let values: Vec<_> = (0..140).map(|i| {
        let needs = if i == 0 { vec![] } else { vec![format!("s{:03}", i - 1)] };
        json!({"id":format!("s{i:03}"),"prompt":if i == 139 { "{{ stages.s000.output }} {{ stages.s064.output }}" } else { "ok" },"needs":needs})
    }).collect();
    assert!(Dag::with_cap(&values, 140).is_ok());
    let error = Dag::validate(&[json!({"id":"a","prompt":"{{ invalid }} {{ unclosed"})])
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "unclosed template");
}

#[test]
fn empty_map_remains_unsettled_when_later_expansion_in_wave_exceeds_cap() {
    let mut dag = Dag::with_cap(&[
        json!({"id":"empty","prompt":"list","schema":"string[]"}),
        json!({"id":"full","prompt":"list","schema":"string[]"}),
        json!({"id":"a_map","prompt":"{{ item }}","kind":"map","over":"empty","needs":["full"]}),
        json!({"id":"z_map","prompt":"{{ item }}","kind":"map","over":"full","needs":["empty"]}),
    ], 4).unwrap();
    dag.start();
    dag.complete(&"empty".into(), Ok("[]".into()));
    let Advance::Done(outcome) = dag.complete(&"full".into(), Ok("[\"one\"]".into())) else {
        panic!()
    };
    assert_eq!(outcome.state, RunState::Failed);
    assert_eq!(
        outcome.error.as_deref(),
        Some("map z_map exceeds stage cap 4")
    );
    assert_eq!(outcome.stages["a_map"], StageStatus::Stopped);
    assert_eq!(dag.output(&"a_map".into()), None);
    assert!(outcome.degraded);
}

#[test]
fn custom_cap_and_reverse_ordered_transitive_dependencies() {
    let mut values: Vec<_> = (0..300).map(|i| {
        let needs = if i == 299 { vec![] } else { vec![format!("s{:03}", i + 1)] };
        json!({"id":format!("s{i:03}"),"prompt":if i == 0 { "{{ stages.s299.output }}" } else { "ok" },"needs":needs})
    }).collect();
    assert!(Dag::with_cap(&values, 300).is_ok());
    values[299]["prompt"] = json!("{{ stages.s000.output }}");
    assert_eq!(
        Dag::with_cap(&values, 300).err().unwrap().to_string(),
        "invalid or non-upstream template `{{ stages.s000.output }}`"
    );
}
