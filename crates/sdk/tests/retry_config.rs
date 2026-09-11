//! Tool and model retry configuration: data failures are classified,
//! non-idempotent mutations are excluded, custom predicates compose with
//! the built-ins, the plain `RetryConfig` still forwards, model retry
//! notifies and honours a live policy, and a child that retries inside
//! is never replayed by the parent's retry layer.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    Context, FnTool, Message, Model, ModelError, ModelResponse, ToolError, ToolSchema,
};
use orca_harness_sdk::orchestration::SubagentModel;
use orca_harness_sdk::{
    Harness, ModelRetryConfig, RetryConfig, Session, SubagentConfig, ToolRetryConfig,
};
use serde_json::{json, Value};

mod common;
use common::temp_dir;

/// A model that fails its first `failures` calls with a transport error,
/// then replays `script`. `ScriptedModel` cannot script errors.
struct FlakyModel {
    failures: AtomicUsize,
    calls: AtomicUsize,
    script: ScriptedModel,
}

impl FlakyModel {
    fn new(failures: usize, script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            failures: AtomicUsize::new(failures),
            calls: AtomicUsize::new(0),
            script: ScriptedModel::new(script),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Model for FlakyModel {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let remaining = self.failures.load(Ordering::SeqCst);
        if remaining > 0 {
            self.failures.store(remaining - 1, Ordering::SeqCst);
            return Err(ModelError::Request("transient".into()));
        }
        self.script.generate(context, tools).await
    }
}

/// A tool named `name` whose `behaviour` decides each invocation's result
/// from the 1-based invocation count; the counter is returned.
fn counting_tool(
    name: &str,
    behaviour: impl Fn(usize) -> Result<Value, ToolError> + Send + Sync + 'static,
) -> (FnTool, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    let behaviour = Arc::new(behaviour);
    let tool = FnTool::new(
        name,
        "counting test tool",
        json!({"type": "object", "properties": {}}),
        move |_args, _ctx| {
            let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
            let behaviour = behaviour.clone();
            async move { behaviour(n) }
        },
    );
    (tool, count)
}

fn always_err(_: usize) -> Result<Value, ToolError> {
    Err(ToolError::msg("boom"))
}

/// The recorded results for `tool` across the whole transcript.
fn tool_results(messages: &[Message], tool: &str) -> Vec<(Value, bool)> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Tool { results } => Some(results),
            _ => None,
        })
        .flatten()
        .filter(|result| result.tool_name == tool)
        .map(|result| (result.output.clone(), result.is_error))
        .collect()
}

async fn run_one_round(
    root: &std::path::Path,
    calls: Vec<orca_harness_core::ToolCall>,
    tools: Vec<FnTool>,
    retry: ToolRetryConfig,
) -> orca_harness_sdk::RunResult {
    let harness = Harness::builder().workspace(root).build().unwrap();
    let mut builder = harness
        .agent(ScriptedModel::tool_round(calls, "done"))
        .tool_retry(retry);
    for tool in tools {
        builder = builder.tool(tool);
    }
    let agent = builder.build().unwrap();
    agent.run("go").await.unwrap()
}

#[tokio::test]
async fn data_failures_are_retried_and_last_output_returned() {
    let root = temp_dir("retry-data");
    let shell_result = |n: usize| Ok(json!({"success": n >= 3, "attempt": n}));

    let (shell, count) = counting_tool("shell", shell_result);
    let result = run_one_round(
        &root,
        vec![call("s1", "shell", json!({}))],
        vec![shell],
        ToolRetryConfig::attempts(3).backoff_ms(0),
    )
    .await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        3,
        "two data failures, then success"
    );
    let results = tool_results(&result.messages, "shell");
    assert_eq!(
        results.len(),
        1,
        "one recorded result however many attempts"
    );
    assert_eq!(results[0].0, json!({"success": true, "attempt": 3}));

    let (shell, count) = counting_tool("shell", shell_result);
    let result = run_one_round(
        &root,
        vec![call("s1", "shell", json!({}))],
        vec![shell],
        ToolRetryConfig::attempts(3)
            .backoff_ms(0)
            .retry_data_failures(false),
    )
    .await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "data failures off: no retry"
    );
    let results = tool_results(&result.messages, "shell");
    assert_eq!(results[0].0, json!({"success": false, "attempt": 1}));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn non_idempotent_mutations_are_not_retried() {
    let root = temp_dir("retry-mutations");
    let (write_file, writes) = counting_tool("write_file", always_err);
    let (grep, greps) = counting_tool("grep", always_err);
    let result = run_one_round(
        &root,
        vec![
            call("w1", "write_file", json!({})),
            call("g1", "grep", json!({})),
        ],
        vec![write_file, grep],
        ToolRetryConfig::attempts(3).backoff_ms(0),
    )
    .await;
    assert_eq!(
        writes.load(Ordering::SeqCst),
        1,
        "write_file errors never replay"
    );
    assert_eq!(
        greps.load(Ordering::SeqCst),
        3,
        "grep errors retry to the cap"
    );
    assert!(tool_results(&result.messages, "write_file")[0].1);
    assert!(tool_results(&result.messages, "grep")[0].1);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn custom_predicates_compose_with_builtins() {
    let root = temp_dir("retry-compose");
    let (write_file, writes) = counting_tool("write_file", always_err);
    let (grep, greps) = counting_tool("grep", always_err);
    let (fetch, fetches) = counting_tool("fetch", always_err);
    let (probe, probes) = counting_tool("probe", |n| Ok(json!({"ok": n >= 2})));
    let (shell, shells) = counting_tool("shell", |n| Ok(json!({"success": n >= 2})));
    run_one_round(
        &root,
        vec![
            call("w1", "write_file", json!({})),
            call("g1", "grep", json!({})),
            call("f1", "fetch", json!({})),
            call("p1", "probe", json!({})),
            call("s1", "shell", json!({})),
        ],
        vec![write_file, grep, fetch, probe, shell],
        ToolRetryConfig::attempts(3)
            .backoff_ms(0)
            .retry_error_when(|call, _| call.name != "grep")
            .retry_ok_when(|call, out| call.name == "probe" && out["ok"] == false),
    )
    .await;
    assert_eq!(greps.load(Ordering::SeqCst), 1, "the custom rule narrows");
    assert_eq!(
        writes.load(Ordering::SeqCst),
        1,
        "the built-in exclusion holds"
    );
    assert_eq!(fetches.load(Ordering::SeqCst), 3, "both rules accept");
    assert_eq!(
        probes.load(Ordering::SeqCst),
        2,
        "the custom data rule adds"
    );
    assert_eq!(
        shells.load(Ordering::SeqCst),
        2,
        "the built-in data rule stays"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn retry_config_forwarding_keeps_old_calls_working() {
    let root = temp_dir("retry-forwarding");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let (count_it, count) = counting_tool("count_it", |n| {
        if n == 1 {
            Err(ToolError::msg("transient"))
        } else {
            Ok(json!({"invocation": n}))
        }
    });
    let model = FlakyModel::new(
        1,
        vec![
            ModelResponse::tool_calls(vec![call("c1", "count_it", json!({}))]),
            ModelResponse::final_text("done"),
        ],
    );
    let agent = harness
        .agent(model.clone())
        .tool(count_it)
        .tool_retry(RetryConfig::attempts(2).backoff_ms(0))
        .model_retry(RetryConfig::attempts(2).backoff_ms(0))
        .build()
        .unwrap();
    let result = agent.run("count").await.unwrap();
    assert_eq!(result.text, "done");
    assert_eq!(count.load(Ordering::SeqCst), 2, "one tool retry");
    assert_eq!(
        model.calls(),
        3,
        "one model retry, then the two-step script"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn model_retry_notifies_and_honors_live_config() {
    let root = temp_dir("retry-model-live");
    let harness = Harness::builder().workspace(&root).build().unwrap();

    let notices = Arc::new(AtomicUsize::new(0));
    let seen = notices.clone();
    let model = FlakyModel::new(2, vec![ModelResponse::final_text("recovered")]);
    let agent = harness
        .agent(model.clone())
        .model_retry(ModelRetryConfig::attempts(3).backoff_ms(0).on_retry(
            move |attempt, max, _error| {
                assert_eq!(max, Some(3));
                assert!(attempt >= 2);
                seen.fetch_add(1, Ordering::SeqCst);
            },
        ))
        .build()
        .unwrap();
    assert_eq!(agent.run("go").await.unwrap().text, "recovered");
    assert_eq!(model.calls(), 3);
    assert_eq!(notices.load(Ordering::SeqCst), 2, "one notice per retry");

    let notices = Arc::new(AtomicUsize::new(0));
    let seen = notices.clone();
    let model = FlakyModel::new(2, vec![ModelResponse::final_text("unreachable")]);
    let agent = harness
        .agent(model.clone())
        .model_retry(
            ModelRetryConfig::attempts(3)
                .backoff_ms(0)
                .on_retry(move |_, _, _| {
                    seen.fetch_add(1, Ordering::SeqCst);
                })
                .live(|| orca_harness_sdk::integrations::ModelRetryConfig {
                    max_attempts: Some(1),
                    backoff: std::time::Duration::ZERO,
                    max_backoff: None,
                }),
        )
        .build()
        .unwrap();
    let error = agent
        .run("go")
        .await
        .expect_err("the live cap stops retries");
    assert!(error.to_string().contains("transient"), "{error}");
    assert_eq!(model.calls(), 1, "the live policy wins over attempts");
    assert_eq!(notices.load(Ordering::SeqCst), 0);

    let _ = std::fs::remove_dir_all(&root);
}

/// The parent delegates once in the foreground, then answers.
fn delegating_parent() -> ScriptedModel {
    ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call(
            "d1",
            "subagent",
            json!({"task": "child work", "background": false}),
        )]),
        ModelResponse::final_text("parent done"),
    ])
}

/// A session whose `subagent` calls run on `child`, with the parent's
/// tools, the given child retry, and the parent's own tool retry.
fn delegating_session(
    root: &std::path::Path,
    child: Arc<dyn Model>,
    tools: Vec<FnTool>,
    child_retry: Option<RetryConfig>,
) -> Session {
    let harness = Harness::builder().workspace(root).build().unwrap();
    let mut subagents =
        SubagentConfig::default().model(SubagentModel::new("flash/child", "child", child));
    if let Some(retry) = child_retry {
        subagents = subagents.tool_retry(retry);
    }
    let mut builder = harness
        .agent(delegating_parent())
        .subagents(subagents)
        .tool_retry(ToolRetryConfig::attempts(3).backoff_ms(0));
    for tool in tools {
        builder = builder.tool(tool);
    }
    let session = builder
        .build()
        .unwrap()
        .new_session()
        .ephemeral()
        .open()
        .unwrap();
    assert!(session
        .subagents()
        .unwrap()
        .settings()
        .set_default_model(Some("flash/child".into())));
    session
}

#[tokio::test]
async fn parent_retry_does_not_replay_delegation_when_children_retry() {
    let root = temp_dir("retry-delegation");
    let child_retry = RetryConfig::attempts(3).backoff_ms(0);

    // A child whose `shell` fails as data twice: retried inside, once.
    let child = Arc::new(ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call("s1", "shell", json!({}))]),
        ModelResponse::final_text("child done"),
    ]));
    let (shell, shells) = counting_tool("shell", |n| Ok(json!({"success": n >= 3})));
    let session = delegating_session(&root, child.clone(), vec![shell], Some(child_retry));
    let result = session.run("delegate").await.unwrap();
    assert_eq!(result.text, "parent done");
    assert_eq!(
        shells.load(Ordering::SeqCst),
        3,
        "the child retried its tool"
    );
    assert_eq!(
        child.generate_calls(),
        2,
        "one child spawn: the parent did not replay"
    );
    let results = tool_results(&result.messages, "subagent");
    assert_eq!(results.len(), 1);
    assert!(!results[0].1, "the child's run succeeded");

    // A child whose run always fails: the parent still does not replay.
    let child = FlakyModel::new(usize::MAX, vec![]);
    let session = delegating_session(&root, child.clone(), vec![], Some(child_retry));
    let result = session.run("delegate").await.unwrap();
    assert_eq!(result.text, "parent done");
    assert_eq!(child.calls(), 1, "one child spawn, one failure");
    let results = tool_results(&result.messages, "subagent");
    assert_eq!(results.len(), 1);
    assert!(
        results[0].1,
        "the failed run reaches the parent model as an error"
    );

    // Without child retry the parent owns the failure and replays the run.
    let child = FlakyModel::new(usize::MAX, vec![]);
    let session = delegating_session(&root, child.clone(), vec![], None);
    let result = session.run("delegate").await.unwrap();
    assert_eq!(result.text, "parent done");
    assert_eq!(
        child.calls(),
        3,
        "each parent attempt spawns the child again"
    );
    assert_eq!(tool_results(&result.messages, "subagent").len(), 1);

    let _ = std::fs::remove_dir_all(&root);
}

/// Time is paused, so the run's elapsed virtual time is exactly the retry
/// backoff slept: zero when the call was not replayed.
#[tokio::test(start_paused = true)]
async fn workflow_run_calls_follow_the_same_rule() {
    let root = temp_dir("retry-workflow");
    let harness = Harness::builder().workspace(&root).build().unwrap();
    let backoff_ms = 1_000;
    let parent = || {
        ScriptedModel::new(vec![
            ModelResponse::tool_calls(vec![call("w1", "workflow", json!({"action": "run"}))]),
            ModelResponse::final_text("parent done"),
        ])
    };
    let run = |child_retry: Option<RetryConfig>| {
        let mut subagents = SubagentConfig::default();
        if let Some(retry) = child_retry {
            subagents = subagents.tool_retry(retry);
        }
        let agent = harness
            .agent(parent())
            .subagents(subagents)
            .tool_retry(ToolRetryConfig::attempts(3).backoff_ms(backoff_ms))
            .build()
            .unwrap();
        async move {
            let started = tokio::time::Instant::now();
            let result = agent.run("submit").await.unwrap();
            let results = tool_results(&result.messages, "workflow");
            assert_eq!(results.len(), 1);
            assert!(results[0].1, "the malformed submission is an error");
            started.elapsed().as_millis() as u64
        }
    };

    let with_child_retry = run(Some(RetryConfig::attempts(3).backoff_ms(0))).await;
    assert!(
        with_child_retry < backoff_ms,
        "no backoff slept: the workflow run was not replayed ({with_child_retry} ms)"
    );
    let without_child_retry = run(None).await;
    assert!(
        without_child_retry >= 2 * backoff_ms,
        "parity without child retry: two backoffs slept ({without_child_retry} ms)"
    );

    let _ = std::fs::remove_dir_all(&root);
}
