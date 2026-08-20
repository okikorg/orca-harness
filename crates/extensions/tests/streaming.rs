//! EventStream forwards model deltas as typed events, so a host can render
//! text as it is generated instead of waiting for the full response.

use std::time::Duration;

use tokio::time::timeout;

use orca_harness_core::testing::ScriptedModel;
use orca_harness_core::{Agent, ModelDelta, ModelResponse};
use orca_harness_extensions::{EventStream, HarnessEvent};

const RUN_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn event_stream_emits_deltas_before_the_full_message() {
    let model =
        ScriptedModel::new(vec![ModelResponse::final_text("Hello world")]).with_deltas(vec![vec![
            ModelDelta::Reasoning { text: "hmm".into() },
            ModelDelta::Text {
                text: "Hello".into(),
            },
            ModelDelta::Text {
                text: " world".into(),
            },
        ]]);

    let (stream, mut rx) = EventStream::channel();
    let agent = Agent::new(model).extension(stream);
    timeout(RUN_TIMEOUT, agent.run("hi"))
        .await
        .unwrap()
        .unwrap();

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    let tags: Vec<String> = events
        .iter()
        .map(|e| match e {
            HarnessEvent::AgentStart => "start".into(),
            HarnessEvent::ReasoningDelta { text } => format!("reasoning:{text}"),
            HarnessEvent::AssistantDelta { text } => format!("delta:{text}"),
            HarnessEvent::Assistant { message } => format!("assistant:{message}"),
            HarnessEvent::Result { .. } => "result".into(),
            other => panic!("unexpected event: {other:?}"),
        })
        .collect();
    assert_eq!(
        tags,
        vec![
            "start",
            "reasoning:hmm",
            "delta:Hello",
            "delta: world",
            "assistant:Hello world",
            "result",
        ]
    );
}

#[tokio::test]
async fn delta_events_serialize_to_platform_ndjson_tags() {
    let ev = HarnessEvent::AssistantDelta {
        text: "chunk".into(),
    };
    let json = serde_json::to_value(&ev).unwrap();
    assert_eq!(json["type"], "assistant_delta");
    assert_eq!(json["text"], "chunk");

    let ev = HarnessEvent::ReasoningDelta { text: "hmm".into() };
    let json = serde_json::to_value(&ev).unwrap();
    assert_eq!(json["type"], "reasoning_delta");
}
