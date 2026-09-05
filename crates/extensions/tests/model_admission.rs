use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use orca_harness_core::{
    Context, DeltaSink, Model, ModelDelta, ModelError, ModelResponse, ToolSchema,
};
use orca_harness_extensions::{ModelGate, RetryModel};
use orca_harness_model_providers::http_error::{request_error, retry_delay};

#[derive(Default)]
struct Provider {
    active: AtomicUsize,
    peak: AtomicUsize,
    calls: AtomicUsize,
    cap: Option<usize>,
}
struct Active<'a>(&'a AtomicUsize);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl Model for Provider {
    async fn generate(&self, _: &Context, _: &[ToolSchema]) -> Result<ModelResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = Active(&self.active);
        self.peak.fetch_max(active, Ordering::SeqCst);
        if self.cap.is_some_and(|cap| active > cap) {
            return Err(request_error(429, None, "busy"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(ModelResponse::final_text("correct"))
    }
}

#[tokio::test(start_paused = true)]
async fn separately_wrapped_workers_share_stream_admission_without_rejections() {
    let provider = Arc::new(Provider {
        cap: Some(16),
        ..Default::default()
    });
    let gate = ModelGate::new(16);
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..64 {
        let model = RetryModel::new(provider.clone(), 1).gate(gate.clone());
        jobs.spawn(async move {
            for _ in 0..4 {
                let response = model.generate(&Context::new(), &[]).await.unwrap();
                assert!(matches!(response, ModelResponse::Final { text, .. } if text == "correct"));
            }
        });
    }
    while let Some(job) = jobs.join_next().await {
        job.unwrap();
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 256);
    assert_eq!(provider.peak.load(Ordering::SeqCst), 16);
    assert_eq!(provider.active.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn aborting_queued_and_active_calls_does_not_leak_permits() {
    let provider = Arc::new(Provider::default());
    let model = Arc::new(RetryModel::new(provider.clone(), 1).gate(ModelGate::new(1)));
    let spawn = || {
        let model = model.clone();
        tokio::spawn(async move { model.generate(&Context::new(), &[]).await })
    };
    let active = spawn();
    tokio::task::yield_now().await;
    let queued = spawn();
    tokio::task::yield_now().await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    queued.abort();
    assert!(queued.await.unwrap_err().is_cancelled());
    active.abort();
    assert!(active.await.unwrap_err().is_cancelled());
    assert_eq!(provider.active.load(Ordering::SeqCst), 0);
    model.generate(&Context::new(), &[]).await.unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
}

struct ThrottledOnce(AtomicUsize);
#[async_trait]
impl Model for ThrottledOnce {
    async fn generate(&self, _: &Context, _: &[ToolSchema]) -> Result<ModelResponse, ModelError> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(request_error(429, Some("10"), "busy"))
        } else {
            Ok(ModelResponse::final_text("ok"))
        }
    }
}

#[tokio::test(start_paused = true)]
async fn final_throttle_cools_down_other_wrappers_and_wait_is_cancellable() {
    let gate = ModelGate::new(None);
    let model = RetryModel::new(ThrottledOnce(AtomicUsize::new(0)), 1)
        .gate(gate.clone())
        .retry_delay(retry_delay);
    assert!(model.generate(&Context::new(), &[]).await.is_err());
    let provider = Arc::new(Provider::default());
    let peer = Arc::new(RetryModel::new(provider.clone(), 1).gate(gate));
    let waiting = {
        let peer = peer.clone();
        tokio::spawn(async move { peer.generate(&Context::new(), &[]).await })
    };
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(9)).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    let start = tokio::time::Instant::now();
    peer.generate(&Context::new(), &[]).await.unwrap();
    assert!(start.elapsed() >= Duration::from_secs(1));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

struct PartialStream(AtomicUsize);
#[async_trait]
impl Model for PartialStream {
    async fn generate(&self, _: &Context, _: &[ToolSchema]) -> Result<ModelResponse, ModelError> {
        unreachable!()
    }
    async fn generate_streaming(
        &self,
        _: &Context,
        _: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        sink.emit(ModelDelta::ToolInput {
            text: "partial".into(),
        })
        .await;
        Err(ModelError::Request("connection dropped".into()))
    }
}

#[tokio::test]
async fn emitted_stream_is_never_replayed_and_releases_its_permit() {
    let inner = Arc::new(PartialStream(AtomicUsize::new(0)));
    let gate = ModelGate::new(1);
    let model = RetryModel::new(inner.clone(), 10)
        .gate(gate.clone())
        .backoff(Duration::ZERO);
    let deltas = Mutex::new(Vec::new());
    let sink = |delta| deltas.lock().unwrap().push(delta);
    assert!(model
        .generate_streaming(&Context::new(), &[], &sink)
        .await
        .is_err());
    assert_eq!(inner.0.load(Ordering::SeqCst), 1);
    assert_eq!(deltas.lock().unwrap().len(), 1);
    let peer = RetryModel::new(Provider::default(), 1).gate(gate);
    tokio::time::timeout(Duration::from_secs(1), peer.generate(&Context::new(), &[]))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn permanent_quota_does_not_retry() {
    struct Quota(AtomicUsize);
    #[async_trait]
    impl Model for Quota {
        async fn generate(
            &self,
            _: &Context,
            _: &[ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(request_error(
                429,
                Some("60"),
                r#"{"error":{"code":"insufficient_quota"}}"#,
            ))
        }
    }
    let inner = Arc::new(Quota(AtomicUsize::new(0)));
    let model = RetryModel::new(inner.clone(), 10).retry_delay(retry_delay);
    assert!(model.generate(&Context::new(), &[]).await.is_err());
    assert_eq!(inner.0.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn caller_deadline_interrupts_provider_cooldown() {
    let gate = ModelGate::new(1);
    let model = RetryModel::new(ThrottledOnce(AtomicUsize::new(0)), None)
        .gate(gate.clone())
        .retry_delay(retry_delay);
    let start = tokio::time::Instant::now();
    let agent = orca_harness_core::Agent::new(model).limits(orca_harness_core::Limits {
        deadline: Some(start + Duration::from_secs(2)),
        ..Default::default()
    });
    assert!(matches!(
        agent.run("go").await,
        Err(orca_harness_core::HarnessError::DeadlineExceeded)
    ));
    assert_eq!(start.elapsed(), Duration::from_secs(2));
    let peer = RetryModel::new(Provider::default(), 1).gate(gate);
    peer.generate(&Context::new(), &[]).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn retries_use_bounded_exponential_jitter() {
    struct Failed(Mutex<Vec<tokio::time::Instant>>);
    #[async_trait]
    impl Model for Failed {
        async fn generate(
            &self,
            _: &Context,
            _: &[ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            self.0.lock().unwrap().push(tokio::time::Instant::now());
            Err(ModelError::Request("offline".into()))
        }
    }
    fastrand::seed(42);
    let inner = Arc::new(Failed(Mutex::new(Vec::new())));
    let model = RetryModel::new(inner.clone(), 10).backoff(Duration::from_secs(1));
    assert!(model.generate(&Context::new(), &[]).await.is_err());
    let calls = inner.0.lock().unwrap();
    assert_eq!(calls.len(), 10);
    let delays: Vec<_> = calls.windows(2).map(|pair| pair[1] - pair[0]).collect();
    for (attempt, delay) in delays.iter().enumerate() {
        assert!(*delay <= Duration::from_secs(1 << attempt) + Duration::from_millis(1));
    }
    assert!(delays.windows(2).any(|pair| pair[0] != pair[1]));
    assert!(delays.iter().any(|delay| *delay > Duration::from_secs(30)));
}

#[tokio::test(start_paused = true)]
async fn unlimited_retries_recover_after_more_than_ten_attempts() {
    struct Recover(AtomicUsize);
    #[async_trait]
    impl Model for Recover {
        async fn generate(
            &self,
            _: &Context,
            _: &[ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            if self.0.fetch_add(1, Ordering::SeqCst) < 12 {
                Err(request_error(429, None, "busy"))
            } else {
                Ok(ModelResponse::final_text("recovered"))
            }
        }
    }
    let inner = Arc::new(Recover(AtomicUsize::new(0)));
    let model = RetryModel::new(inner.clone(), None)
        .backoff(Duration::ZERO)
        .gate(ModelGate::new(None))
        .retry_delay(retry_delay);
    model.generate(&Context::new(), &[]).await.unwrap();
    assert_eq!(inner.0.load(Ordering::SeqCst), 13);
}

#[tokio::test(start_paused = true)]
async fn unconfigured_gate_does_not_impose_a_stream_ceiling() {
    let provider = Arc::new(Provider::default());
    let gate = ModelGate::new(None);
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..64 {
        let model = RetryModel::new(provider.clone(), None).gate(gate.clone());
        jobs.spawn(async move {
            model.generate(&Context::new(), &[]).await.unwrap();
        });
    }
    while let Some(job) = jobs.join_next().await {
        job.unwrap();
    }
    assert_eq!(provider.peak.load(Ordering::SeqCst), 64);
    assert_eq!(provider.active.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn live_provider_limit_releases_waiters_and_lowering_waits_for_active_calls() {
    let (limit, watch) = tokio::sync::watch::channel(1);
    let provider = Arc::new(Provider::default());
    let model = Arc::new(RetryModel::new(provider.clone(), 1).gate(ModelGate::from_limit(watch)));
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..5 {
        let model = model.clone();
        jobs.spawn(async move {
            model.generate(&Context::new(), &[]).await.unwrap();
        });
    }
    tokio::task::yield_now().await;
    assert_eq!(provider.active.load(Ordering::SeqCst), 1);
    limit.send_replace(3);
    tokio::task::yield_now().await;
    assert_eq!(provider.active.load(Ordering::SeqCst), 3);
    limit.send_replace(1);
    tokio::task::yield_now().await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    tokio::time::advance(Duration::from_millis(20)).await;
    tokio::task::yield_now().await;
    assert_eq!(provider.active.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
    limit.send_replace(0);
    tokio::task::yield_now().await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 5);
    while let Some(job) = jobs.join_next().await {
        job.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn live_model_policy_sets_attempts_delay_and_optional_ceiling() {
    use orca_harness_extensions::ModelRetryConfig;
    struct Failed(Mutex<Vec<tokio::time::Instant>>);
    #[async_trait]
    impl Model for Failed {
        async fn generate(
            &self,
            _: &Context,
            _: &[ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            self.0.lock().unwrap().push(tokio::time::Instant::now());
            Err(ModelError::Request("busy".into()))
        }
    }
    let inner = Arc::new(Failed(Mutex::new(Vec::new())));
    let policy = Arc::new(Mutex::new(ModelRetryConfig {
        max_attempts: Some(4),
        backoff: Duration::from_secs(100),
        max_backoff: Some(Duration::from_millis(10)),
    }));
    let live = policy.clone();
    let model = RetryModel::new(inner.clone(), 1).config(move || *live.lock().unwrap());
    assert!(model.generate(&Context::new(), &[]).await.is_err());
    {
        let calls = inner.0.lock().unwrap();
        assert_eq!(calls.len(), 4);
        assert!(calls
            .windows(2)
            .all(|pair| pair[1] - pair[0] <= Duration::from_millis(11)));
    }
    policy.lock().unwrap().max_attempts = Some(2);
    assert!(model.generate(&Context::new(), &[]).await.is_err());
    assert_eq!(inner.0.lock().unwrap().len(), 6);

    let live_inner = Arc::new(Failed(Mutex::new(Vec::new())));
    let live_policy = Arc::new(Mutex::new(ModelRetryConfig {
        max_attempts: None,
        backoff: Duration::ZERO,
        max_backoff: None,
    }));
    let read_policy = live_policy.clone();
    let live_model = RetryModel::new(live_inner.clone(), None)
        .config(move || *read_policy.lock().unwrap())
        .on_retry(move |_, _, _| {
            live_policy.lock().unwrap().max_attempts = Some(2);
        });
    assert!(live_model.generate(&Context::new(), &[]).await.is_err());
    assert_eq!(
        live_inner.0.lock().unwrap().len(),
        2,
        "an in-progress request sees policy edits"
    );

    let throttled = RetryModel::new(ThrottledOnce(AtomicUsize::new(0)), 1)
        .config(move || *policy.lock().unwrap())
        .retry_delay(retry_delay);
    let start = tokio::time::Instant::now();
    throttled.generate(&Context::new(), &[]).await.unwrap();
    assert!(
        start.elapsed() >= Duration::from_secs(10),
        "Retry-After overrides the user backoff ceiling"
    );
}
