#[tokio::test]
async fn tool_policy_allowlist_denies_unlisted() {
    let model = Arc::new(ScriptedModel::tool_round(
        vec![
            call("c0", "echo", json!({"a": 1})),
            call("c1", "secret", json!({})),
        ],
        "done",
    ));
    let secret = FnTool::new(
        "secret",
        "should be blocked",
        json!({"type": "object"}),
        |_i, _c| async move { Ok(json!("leaked")) },
    );
    let agent = Agent::new(model.clone())
        .tool(echo())
        .tool(secret)
        .extension(ToolPolicy::new().allow(["echo"]));
    timeout(RUN_TIMEOUT, agent.run("try"))
        .await
        .unwrap()
        .unwrap();

    let seen = model.observed_contexts();
    let results = last_tool_results(&seen);
    assert!(!results[0].is_error, "echo allowed");
    assert!(results[1].is_error, "secret denied");
    assert!(results[1].output["error"]
        .as_str()
        .unwrap()
        .contains("not in the allowlist"));
}

#[tokio::test]
async fn tool_policy_custom_rule() {
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("c0", "echo", json!({"danger": true}))],
        "done",
    ));
    let policy = ToolPolicy::new().rule(|c: &orca_harness_core::ToolCall| {
        if c.arguments["danger"].as_bool() == Some(true) {
            PolicyOutcome::Deny("dangerous args".into())
        } else {
            PolicyOutcome::Allow
        }
    });
    let agent = Agent::new(model.clone()).tool(echo()).extension(policy);
    timeout(RUN_TIMEOUT, agent.run("try"))
        .await
        .unwrap()
        .unwrap();
    let results = last_tool_results(&model.observed_contexts());
    assert!(results[0].is_error);
    assert!(results[0].output["error"]
        .as_str()
        .unwrap()
        .contains("dangerous args"));
}

#[tokio::test]
async fn truncation_caps_large_output() {
    let big = "x".repeat(1000);
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("c0", "echo", json!({"text": big}))],
        "done",
    ));
    let agent = Agent::new(model.clone())
        .tool(echo())
        .extension(Truncation::new(100));
    timeout(RUN_TIMEOUT, agent.run("truncate"))
        .await
        .unwrap()
        .unwrap();

    let results = last_tool_results(&model.observed_contexts());
    let text = results[0].output["text"].as_str().unwrap();
    assert!(
        text.chars().count() < 300,
        "should be truncated, got {}",
        text.chars().count()
    );
    assert!(text.contains("elided"));
    assert_eq!(results[0].output["_truncated"], json!(true));
}

#[tokio::test]
async fn tool_retry_recovers_after_transient_failure() {
    let attempts = Arc::new(AtomicU32::new(0));
    let flaky = {
        let attempts = attempts.clone();
        FnTool::new(
            "flaky",
            "fails first, then succeeds",
            json!({"type": "object"}),
            move |_i, _c| {
                let attempts = attempts.clone();
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    if n < 3 {
                        Err(ToolError::msg("transient"))
                    } else {
                        Ok(json!({"ok": n}))
                    }
                }
            },
        )
    };
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("c0", "flaky", json!({}))],
        "done",
    ));
    let agent = Agent::new(model.clone())
        .tool(flaky)
        .extension(ToolRetry::new(3).backoff(Duration::from_millis(1)));
    timeout(RUN_TIMEOUT, agent.run("retry"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let results = last_tool_results(&model.observed_contexts());
    assert!(!results[0].is_error, "third attempt succeeds");
    assert_eq!(results[0].output["ok"], json!(3));
}

#[tokio::test]
async fn tool_retry_exhausts_and_reports_error() {
    let always = FnTool::new(
        "always",
        "always fails",
        json!({"type": "object"}),
        |_i, _c| async move { Err(ToolError::msg("nope")) },
    );
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("c0", "always", json!({}))],
        "done",
    ));
    let agent = Agent::new(model.clone())
        .tool(always)
        .extension(ToolRetry::new(2).backoff(Duration::from_millis(1)));
    timeout(RUN_TIMEOUT, agent.run("retry"))
        .await
        .unwrap()
        .unwrap();
    let results = last_tool_results(&model.observed_contexts());
    assert!(results[0].is_error);
    assert!(results[0].output["error"]
        .as_str()
        .unwrap()
        .contains("nope"));
}

#[tokio::test]
async fn tool_retry_error_rule_skips_non_retryable_failure() {
    let attempts = Arc::new(AtomicU32::new(0));
    let deterministic = {
        let attempts = attempts.clone();
        FnTool::new(
            "multi_edit",
            "returns a deterministic matching error",
            json!({"type": "object"}),
            move |_input, _ctx| {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err(ToolError::msg("`old` occurs 4 times"))
                }
            },
        )
    };
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("c0", "multi_edit", json!({}))],
        "done",
    ));
    let agent = Agent::new(model.clone()).tool(deterministic).extension(
        ToolRetry::new(3)
            .backoff(Duration::from_secs(1))
            .retry_error_when(|call, _error| call.name != "multi_edit"),
    );

    timeout(RUN_TIMEOUT, agent.run("retry"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let results = last_tool_results(&model.observed_contexts());
    assert!(results[0].is_error);
    assert!(results[0].output["error"]
        .as_str()
        .unwrap()
        .contains("occurs 4 times"));
}

#[tokio::test]
async fn tool_retry_retries_data_failures_and_reports_last_output() {
    // A tool whose failures are *data*: it reports `success: false`
    // instead of returning Err. This is what shell/web_fetch do for
    // nonzero exits and 5xx responses — the classic case retry must
    // cover to be useful.
    let attempts = Arc::new(AtomicU32::new(0));
    let flaky = {
        let attempts = attempts.clone();
        FnTool::new(
            "probe",
            "reports success: false until the third try",
            json!({"type": "object"}),
            move |_i, _c| {
                let attempts = attempts.clone();
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    Ok(json!({"success": n >= 3, "attempt": n}))
                }
            },
        )
    };
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("c0", "probe", json!({}))],
        "done",
    ));

    // Three total attempts; every `success: false` output is retried.
    let agent = Agent::new(model.clone()).tool(flaky).extension(
        ToolRetry::new(3)
            .backoff(Duration::from_millis(1))
            .retry_ok_when(|call, out| {
                call.name == "probe" && out["success"].as_bool() == Some(false)
            }),
    );
    timeout(RUN_TIMEOUT, agent.run("retry"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let results = last_tool_results(&model.observed_contexts());
    assert!(
        !results[0].is_error,
        "rule-last output flows back as a result"
    );
    assert_eq!(results[0].output["attempt"], json!(3));

    // When every attempt is data-failed, the last real output (not a
    // synthetic error) is what the model sees.
    let always_fail = FnTool::new(
        "probe",
        "always success: false",
        json!({"type": "object"}),
        |_i, _c| async move { Ok(json!({"success": false, "status": 503})) },
    );
    let model = Arc::new(ScriptedModel::tool_round(
        vec![call("c0", "probe", json!({}))],
        "done",
    ));
    let agent = Agent::new(model.clone()).tool(always_fail).extension(
        ToolRetry::new(2)
            .backoff(Duration::from_millis(1))
            .retry_ok_when(|_, out| out["success"].as_bool() == Some(false)),
    );
    timeout(RUN_TIMEOUT, agent.run("retry"))
        .await
        .unwrap()
        .unwrap();
    let results = last_tool_results(&model.observed_contexts());
    assert!(!results[0].is_error, "last output is returned as-is");
    assert_eq!(results[0].output["status"], json!(503));
}

/// A model that fails transiently N times before succeeding.
struct FlakyModel {
    fails_remaining: AtomicU32,
    log: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Model for FlakyModel {
    async fn generate(
        &self,
        _context: &orca_harness_core::Context,
        _tools: &[orca_harness_core::ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.log.lock().unwrap().push("gen".into());
        if self.fails_remaining.fetch_sub(1, Ordering::SeqCst) > 0 {
            Err(ModelError::Request("503 upstream".into()))
        } else {
            Ok(ModelResponse::final_text("recovered"))
        }
    }
}
