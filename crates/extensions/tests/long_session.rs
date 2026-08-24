use orca_harness_core::{Context, Extension, ModelResponse, Usage};
use orca_harness_extensions::{ContextCapacity, LongSession, LongSessionConfig, TruncationStore};

fn large_context() -> Context {
    let mut context = Context::new();
    context.push_system("system");
    for index in 0..8 {
        context.push_user(format!("request {index} {}", "x".repeat(1_000)));
        context.push_assistant_text(format!("answer {index}"));
    }
    context
}

async fn report_usage(extension: &LongSession, tokens: u64) {
    extension
        .after_model(
            &mut Context::new(),
            &ModelResponse::Final {
                text: String::new(),
                usage: Some(Usage {
                    input_tokens: tokens,
                    ..Default::default()
                }),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn compacts_when_reported_usage_crosses_dynamic_capacity() {
    let capacity = ContextCapacity::new(Some(1_000));
    let extension =
        LongSession::new(capacity, TruncationStore::default()).config(LongSessionConfig {
            compact_at_percent: 80,
            tail_percent: 20,
        });
    let mut context = large_context();
    let before = context.messages().len();

    report_usage(&extension, 850).await;
    extension.before_model(&mut context).await.unwrap();

    assert!(context.messages().len() < before);
}

#[tokio::test]
async fn resumed_context_compacts_before_its_first_model_step() {
    let extension = LongSession::new(
        ContextCapacity::new(Some(1_000)),
        TruncationStore::default(),
    );
    let mut context = large_context();
    let before = context.messages().len();

    extension.before_model(&mut context).await.unwrap();

    assert!(context.messages().len() < before);
}

#[tokio::test]
async fn unknown_capacity_never_guesses_a_model_limit() {
    let extension = LongSession::new(ContextCapacity::default(), TruncationStore::default());
    let mut context = large_context();
    let before = serde_json::to_value(context.messages()).unwrap();

    report_usage(&extension, 1_000_000).await;
    extension.before_model(&mut context).await.unwrap();

    assert_eq!(serde_json::to_value(context.messages()).unwrap(), before);
}

#[tokio::test]
async fn capacity_can_change_when_the_host_switches_models() {
    let capacity = ContextCapacity::new(Some(10_000));
    let extension = LongSession::new(capacity.clone(), TruncationStore::default());
    let mut context = large_context();
    let before = context.messages().len();

    report_usage(&extension, 900).await;
    extension.before_model(&mut context).await.unwrap();
    assert_eq!(context.messages().len(), before);

    capacity.set(Some(1_000));
    extension.before_model(&mut context).await.unwrap();
    assert!(context.messages().len() < before);
}

#[test]
fn capacity_round_trips_without_a_model_table() {
    let capacity = ContextCapacity::new(None);
    assert_eq!(capacity.get(), None);
    capacity.set(Some(128_000));
    assert_eq!(capacity.get(), Some(128_000));
    capacity.set(None);
    assert_eq!(capacity.get(), None);
}

#[test]
fn stale_capacity_probe_cannot_overwrite_a_newer_model() {
    let capacity = ContextCapacity::new(Some(8_000));
    let old = capacity.begin_update();
    let current = capacity.begin_update();

    assert!(!capacity.finish_update(old, Some(16_000)));
    assert!(capacity.finish_update(current, Some(128_000)));
    assert_eq!(capacity.get(), Some(128_000));
}
