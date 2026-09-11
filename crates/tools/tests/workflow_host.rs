//! Typed workflow operations run the same implementation as the `workflow`
//! model tool: identical validation before admission, identical
//! acknowledgements and outputs, and the same runtime for dependencies,
//! maps, caps, timeouts, reuse, inspection, and cancellation.

use orca_harness_core::{CancellationToken, Tool};
use orca_harness_dag::{Kind, RunState, StageStatus};
use orca_harness_tools::{BackgroundStatus, StageTiming, WorkflowSubmission};
use serde_json::{json, Value};
use std::time::Duration;

mod workflow_support;
use workflow_support::{ctx, stage, terminal, tool, wait_until};

fn tool_input(stages: &[orca_harness_dag::Stage]) -> Value {
    json!({"action":"run","graph":serde_json::to_value(stages).unwrap()})
}

#[tokio::test]
async fn typed_submit_matches_tool_run_json() {
    let (tool, manager, mut rx, _, _) = tool(None, 4);
    let stages = vec![
        stage("a", "alpha", &[]),
        stage("b", "beta {{ stages.a.output }}", &["a"]),
    ];
    let typed = tool
        .submit(WorkflowSubmission::new(stages.clone()))
        .unwrap();
    let (typed_done, typed_stages) = terminal(&mut rx).await;
    manager.acknowledge(typed_done.generation, typed_done.spawn.id);
    let via_tool = tool.call(tool_input(&stages), &ctx()).await.unwrap();
    let (tool_done, tool_stages) = terminal(&mut rx).await;

    assert_eq!(typed.stages, 2);
    let mut ack = typed.clone().into_value();
    assert_eq!(ack["runId"], typed.run_id);
    ack["runId"] = via_tool["runId"].clone();
    assert_eq!(ack, via_tool, "one acknowledgement shape");
    assert_eq!(typed_done.spawn.call_id, "host:workflow");
    assert_eq!(tool_done.spawn.call_id, "workflow-call");
    assert_eq!((typed_stages, tool_stages), (2, 2));
    let outputs = |n: &orca_harness_tools::SubagentNotification| {
        n.result.as_ref().unwrap()["workflow"]["outputs"].clone()
    };
    assert_eq!(outputs(&typed_done), json!({"b":"beta alpha"}));
    assert_eq!(outputs(&typed_done), outputs(&tool_done));
}

#[tokio::test]
async fn invalid_graph_fails_before_admission_on_both_paths() {
    let (tool, manager, _, calls, store) = tool(None, 4);
    let cases: Vec<(&str, WorkflowSubmission, Value, &str)> = vec![
        (
            "cycle",
            WorkflowSubmission::new([stage("a", "a", &["b"]), stage("b", "b", &["a"])]),
            tool_input(&[stage("a", "a", &["b"]), stage("b", "b", &["a"])]),
            "cycle involving",
        ),
        (
            "unknown dependency",
            WorkflowSubmission::new([stage("a", "a", &["missing"])]),
            tool_input(&[stage("a", "a", &["missing"])]),
            "unknown dependency `missing`",
        ),
        (
            "empty prompt",
            WorkflowSubmission::new([stage("a", "  ", &[])]),
            tool_input(&[stage("a", "  ", &[])]),
            "a: empty prompt",
        ),
        (
            "duplicate id",
            WorkflowSubmission::new([stage("a", "a", &[]), stage("a", "again", &[])]),
            tool_input(&[stage("a", "a", &[]), stage("a", "again", &[])]),
            "duplicate stage `a`",
        ),
        (
            "over cap",
            WorkflowSubmission::new([stage("a", "a", &[]), stage("b", "b", &[])]).max_stages(1),
            {
                let mut input = tool_input(&[stage("a", "a", &[]), stage("b", "b", &[])]);
                input["maxStages"] = json!(1);
                input
            },
            "graph must contain 1..=1 stages",
        ),
        (
            "unknown model",
            WorkflowSubmission::new([{
                let mut s = stage("a", "a", &[]);
                s.model = Some("unknown".into());
                s
            }]),
            json!({"action":"run","graph":[{"id":"a","prompt":"a","model":"unknown"}]}),
            "unknown subagent model `unknown`",
        ),
    ];
    for (name, submission, input, message) in cases {
        let typed = tool.submit(submission).unwrap_err().to_string();
        let via_tool = tool.call(input, &ctx()).await.unwrap_err().to_string();
        assert!(typed.contains(message), "{name}: typed said {typed}");
        assert_eq!(typed, via_tool, "{name}: one message on both paths");
    }
    assert!(tool
        .submit(WorkflowSubmission::new([stage("a", "a", &[])]).max_stages(0))
        .unwrap_err()
        .to_string()
        .contains("maxStages must be a positive integer"));
    assert!(tool
        .submit(WorkflowSubmission::new([stage("a", "a", &[])]).timeout(Duration::ZERO))
        .unwrap_err()
        .to_string()
        .contains("timeoutSeconds must be positive"));
    assert!(tool
        .submit(WorkflowSubmission::new([stage("a", "a", &[])]).resume_from(99))
        .unwrap_err()
        .to_string()
        .contains("resumeFrom run does not exist"));
    for input in [
        json!({"action":"run","graph":[{"id":"a","prompt":"a"}],"maxStages":0}),
        json!({"action":"run","graph":[{"id":"a","prompt":"a"}],"timeoutSeconds":0}),
    ] {
        let via_tool = tool.call(input, &ctx()).await.unwrap_err().to_string();
        assert!(via_tool.contains("must be"), "{via_tool}");
    }
    assert!(manager.active().is_empty(), "nothing was admitted");
    assert!(calls.lock().unwrap().is_empty(), "no stage ran");
    assert_eq!(store.runs(), 0, "a rejected run records nothing");
    assert!(tool.runs().is_empty());
}

#[tokio::test]
async fn dependencies_maps_and_stage_cap() {
    let (tool, _manager, mut rx, calls, _) = tool(None, 4);
    let mut source = stage("source", "dimensions", &[]);
    source.schema = Some("string[]".into());
    let mut map = stage("m", "review {{ item }}", &[]);
    map.kind = Kind::Map;
    map.over = Some("source".into());
    let ack = tool
        .submit(WorkflowSubmission::new([
            stage("a", "alpha", &[]),
            stage("b", "beta {{ stages.a.output }}", &["a"]),
            source,
            map,
        ]))
        .unwrap();
    let (done, stages) = terminal(&mut rx).await;
    assert_eq!(stages, 5, "a, b, source, and two mapped items");
    let outcome = done.result.unwrap();
    assert_eq!(
        outcome["workflow"]["outputs"],
        json!({"b":"beta alpha","m":"[\"review a\",\"review b\"]"})
    );
    assert!(calls.lock().unwrap().contains(&"beta alpha".to_string()));

    let status = tool
        .status(ack.run_id)
        .expect("finished run is inspectable");
    assert_eq!(status.state, RunState::Done);
    assert!(status.active.is_empty());
    for id in ["a", "b", "source", "m"] {
        assert_eq!(status.stages[id], StageStatus::Done, "{id}");
    }
    assert_eq!(
        serde_json::to_value(&status.outcome.as_ref().unwrap().outputs).unwrap(),
        outcome["workflow"]["outputs"]
    );
    assert_eq!(
        tool.stage_output(ack.run_id, "m").unwrap().answer,
        "[\"review a\",\"review b\"]"
    );

    let capped = tool
        .submit(WorkflowSubmission::new([stage("a", "a", &[]), stage("b", "b", &[])]).max_stages(1))
        .unwrap_err();
    assert!(capped
        .to_string()
        .contains("graph must contain 1..=1 stages"));
    assert!(
        tool.runs().is_empty(),
        "the capped graph was never admitted"
    );
}

#[tokio::test]
async fn timeout_covers_queued_stages() {
    let (tool, manager, mut rx, _, _) = tool(Some(CancellationToken::new()), 4);
    let ack = tool
        .submit(
            WorkflowSubmission::new([stage("a", "held", &[]), stage("b", "queued", &[])])
                .timeout(Duration::from_millis(200)),
        )
        .unwrap();
    let live = tool.status(ack.run_id).unwrap();
    assert_eq!(live.state, RunState::Running);
    assert_eq!(live.active.len(), 2, "one running, one queued");
    assert!(live
        .active
        .iter()
        .any(|job| job.status == BackgroundStatus::Queued));

    let (done, _) = terminal(&mut rx).await;
    assert!(done.result.is_err());
    let status = tool.status(ack.run_id).unwrap();
    assert_eq!(
        status.state,
        RunState::Failed,
        "the running stage timed out, which fails the run"
    );
    assert_eq!(status.stages["a"], StageStatus::Failed);
    assert_eq!(
        status.stages["b"],
        StageStatus::Stopped,
        "the queued stage was abandoned by the failure, not cancelled"
    );
    assert!(status.outcome.is_some());
    manager.cancel_all();
}

#[tokio::test]
async fn resume_from_replays_cached_outputs() {
    let (tool, manager, mut rx, calls, _) = tool(None, 4);
    let graph = || WorkflowSubmission::new([stage("a", "alpha", &[])]);
    let first = tool.submit(graph()).unwrap();
    let (done, stages) = terminal(&mut rx).await;
    assert_eq!(stages, 1);
    manager.acknowledge(done.generation, done.spawn.id);

    let second = tool.submit(graph().resume_from(first.run_id)).unwrap();
    let (done, stages) = terminal(&mut rx).await;
    assert_eq!(stages, 0, "the cached stage did not run");
    assert!(done.result.is_ok());
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "the model was called once in total"
    );
    let status = tool.status(second.run_id).unwrap();
    assert_eq!(status.state, RunState::Done);
    let outcome = status.outcome.unwrap();
    assert_eq!(
        outcome.timings["a"],
        StageTiming::Ran {
            runtime_ms: Some(0),
            cached: true
        }
    );
    assert_eq!(outcome.outputs["a"], "alpha");
    assert_eq!(
        tool.stage_output(second.run_id, "a").unwrap().answer,
        "alpha"
    );
}

#[tokio::test]
async fn status_and_stage_output_during_and_after_run() {
    let release = CancellationToken::new();
    let (tool, _manager, mut rx, _, _) = tool(Some(release.clone()), 4);
    assert!(tool.status(7).is_none());
    let ack = tool
        .submit(WorkflowSubmission::new([stage("a", "held", &[])]))
        .unwrap();

    let live = tool.status(ack.run_id).expect("a live run is inspectable");
    assert_eq!(live.state, RunState::Running);
    assert_eq!(live.stages["a"], StageStatus::Running);
    assert_eq!(live.active.len(), 1);
    assert_eq!(live.active[0].stage.as_deref(), Some("a"));
    assert_eq!(live.active[0].status, BackgroundStatus::Running);
    assert!(live.outcome.is_none());
    assert_eq!(tool.runs(), vec![live.clone()]);
    assert_eq!(tool.runs(), vec![live], "a host may poll freely");
    assert_eq!(
        tool.stage_output(ack.run_id, "a").unwrap_err().to_string(),
        "stage output is unavailable"
    );

    release.cancel();
    let (done, _) = terminal(&mut rx).await;
    assert!(done.result.is_ok());
    let finished = tool.status(ack.run_id).unwrap();
    assert_eq!(finished.state, RunState::Done);
    assert_eq!(finished.stages["a"], StageStatus::Done);
    assert!(finished.active.is_empty());
    assert_eq!(finished.outcome.as_ref().unwrap().state, RunState::Done);
    assert!(tool.runs().is_empty());
    let output = tool.stage_output(ack.run_id, "a").unwrap();
    assert_eq!(output.answer, "held");
    assert_eq!(
        output.clone().into_value(),
        json!({"runId":ack.run_id,"stage":"a","answer":"held"})
    );
    assert_eq!(
        tool.stage_output(ack.run_id, "missing")
            .unwrap_err()
            .to_string(),
        "stage output is unavailable"
    );
    assert_eq!(
        tool.stage_output(ack.run_id + 100, "a")
            .unwrap_err()
            .to_string(),
        "stage output is unavailable"
    );
}

#[tokio::test]
async fn cancel_settles_run_and_children() {
    let (tool, manager, mut rx, _, _) = tool(Some(CancellationToken::new()), 4);
    let ack = tool
        .submit(WorkflowSubmission::new([
            stage("a", "held", &[]),
            stage("b", "held too", &[]),
        ]))
        .unwrap();
    assert_eq!(tool.status(ack.run_id).unwrap().active.len(), 2);

    tool.cancel(ack.run_id).unwrap();
    let (done, _) = terminal(&mut rx).await;
    let error = done.result.unwrap_err();
    assert!(error.starts_with("workflow cancelled"), "{error}");
    let status = tool.status(ack.run_id).unwrap();
    assert_eq!(status.state, RunState::Cancelled);
    assert_eq!(status.stages["a"], StageStatus::Cancelled);
    assert_eq!(status.stages["b"], StageStatus::Cancelled);
    wait_until(|| manager.active().is_empty(), "the children to exit").await;
    assert!(tool.runs().is_empty());
    assert_eq!(
        tool.cancel(ack.run_id).unwrap_err().to_string(),
        "workflow is not running"
    );
    assert_eq!(
        tool.cancel(ack.run_id + 100).unwrap_err().to_string(),
        "workflow is not running"
    );
}
