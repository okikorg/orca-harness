#[tokio::test]
async fn retry_model_retries_transient_request_errors() {
    let model = RetryModel::new(
        FlakyModel {
            fails_remaining: AtomicU32::new(2),
            log: Mutex::new(Vec::new()),
        },
        5,
    )
    .backoff(Duration::from_millis(1));
    let agent = Agent::new(model);
    let answer = timeout(RUN_TIMEOUT, agent.run("go"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer, "recovered");
}

async fn assert_model_error_is_not_retried(error: ModelError) {
    struct AlwaysFails {
        calls: Arc<AtomicU32>,
        error: Mutex<Option<ModelError>>,
    }
    #[async_trait::async_trait]
    impl Model for AlwaysFails {
        async fn generate(
            &self,
            _c: &orca_harness_core::Context,
            _t: &[orca_harness_core::ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(self.error.lock().unwrap().take().unwrap())
        }
    }
    let counter = Arc::new(AtomicU32::new(0));
    let model = RetryModel::new(
        AlwaysFails {
            calls: Arc::clone(&counter),
            error: Mutex::new(Some(error)),
        },
        5,
    )
    .backoff(Duration::from_millis(1));
    let agent = Agent::new(model);
    let result = timeout(RUN_TIMEOUT, agent.run("go")).await.unwrap();
    assert!(result.is_err());
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_model_does_not_retry_invalid_response() {
    assert_model_error_is_not_retried(ModelError::InvalidResponse("garbage".into())).await;
}

#[tokio::test]
async fn retry_model_does_not_retry_authentication_errors() {
    assert_model_error_is_not_retried(ModelError::Authentication("login required".into())).await;
}

#[tokio::test]
async fn retry_model_retries_incomplete_generation_with_a_bound() {
    struct TruncatedThenComplete(AtomicU32);

    #[async_trait::async_trait]
    impl Model for TruncatedThenComplete {
        async fn generate(
            &self,
            _context: &orca_harness_core::Context,
            _tools: &[orca_harness_core::ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(ModelError::IncompleteResponse {
                    message: "stream ended before [DONE]".into(),
                    usage: Some(Usage {
                        output_tokens: 10,
                        ..Default::default()
                    }),
                });
            }
            Ok(ModelResponse::final_text("recovered"))
        }
    }

    let model = RetryModel::new(TruncatedThenComplete(AtomicU32::new(0)), 2)
        .backoff(Duration::from_millis(1));
    let answer = Agent::new(model).run("go").await.unwrap();
    assert_eq!(answer, "recovered");
}

#[tokio::test]
async fn retry_model_does_not_retry_malformed_tool_arguments() {
    assert_model_error_is_not_retried(ModelError::MalformedToolArguments {
        tool_name: "write_file".into(),
        argument_bytes: 12_170,
        finish_reason: Some("tool_calls".into()),
        message: "invalid JSON".into(),
        usage: None,
    })
    .await;
}

#[tokio::test]
async fn retry_model_preserves_streaming_and_retries_request_errors() {
    struct FlakyStreamingModel(Arc<AtomicU32>);

    #[async_trait::async_trait]
    impl Model for FlakyStreamingModel {
        async fn generate(
            &self,
            _context: &orca_harness_core::Context,
            _tools: &[orca_harness_core::ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            panic!("the streaming path must remain streaming");
        }

        async fn generate_streaming(
            &self,
            _context: &orca_harness_core::Context,
            _tools: &[orca_harness_core::ToolSchema],
            sink: &dyn orca_harness_core::DeltaSink,
        ) -> Result<ModelResponse, ModelError> {
            if self.0.fetch_add(1, Ordering::SeqCst) < 2 {
                return Err(ModelError::Request("connection dropped".into()));
            }
            sink.emit(orca_harness_core::ModelDelta::Text {
                text: "recovered".into(),
            })
            .await;
            Ok(ModelResponse::final_text("recovered"))
        }
    }

    let attempts = Arc::new(AtomicU32::new(0));
    let retries = Arc::new(Mutex::new(Vec::new()));
    let deltas = Arc::new(Mutex::new(Vec::new()));
    let sink_deltas = deltas.clone();
    let sink = move |delta| sink_deltas.lock().unwrap().push(delta);
    let retry_log = retries.clone();
    let model = RetryModel::new(FlakyStreamingModel(attempts.clone()), 10)
        .backoff(Duration::from_millis(1))
        .on_retry(move |attempt, max_attempts, _| {
            retry_log.lock().unwrap().push((attempt, max_attempts));
        });

    let response = model
        .generate_streaming(&orca_harness_core::Context::new(), &[], &sink)
        .await
        .unwrap();

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(*retries.lock().unwrap(), vec![(2, Some(10)), (3, Some(10))]);
    assert!(matches!(response, ModelResponse::Final { ref text, .. } if text == "recovered"));
    assert_eq!(deltas.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn usage_meter_accumulates_across_steps() {
    let model = ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: None,
            calls: vec![call("c0", "echo", json!({}))],
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: 5,
                ..Default::default()
            }),
        },
        ModelResponse::Final {
            text: "done".into(),
            usage: Some(Usage {
                input_tokens: 50,
                output_tokens: 10,
                ..Default::default()
            }),
        },
    ]);
    let (meter, usage) = UsageMeter::new();
    let agent = Agent::new(model).tool(echo()).extension(meter);
    timeout(RUN_TIMEOUT, agent.run("go"))
        .await
        .unwrap()
        .unwrap();

    let total = usage.total();
    assert_eq!(total.input_tokens, 150);
    assert_eq!(total.output_tokens, 30);
    assert_eq!(total.cache_read_tokens, 5);
    assert_eq!(usage.metered_steps(), 2);
}

fn last_tool_results(
    contexts: &[orca_harness_core::Context],
) -> Vec<orca_harness_core::ToolResult> {
    contexts
        .last()
        .unwrap()
        .messages()
        .iter()
        .rev()
        .find_map(|m| match m {
            orca_harness_core::Message::Tool { results } => Some(results.clone()),
            _ => None,
        })
        .expect("tool results present")
}

/// Keep Value import used across cfgs.
#[allow(dead_code)]
fn _touch() -> Value {
    Value::Null
}
