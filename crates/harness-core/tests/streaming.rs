//! Streaming deltas: models can emit incremental fragments to subscribed
//! extensions while a response is being produced. The returned
//! `ModelResponse` stays authoritative; deltas are presentation-only.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::time::timeout;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{
    Agent, Context, Extension, FnTool, Model, ModelDelta, ModelError, ModelResponse, Subscriptions,
    ToolSchema,
};

const RUN_TIMEOUT: Duration = Duration::from_secs(10);

/// Records every delta it receives.
struct DeltaRecorder {
    deltas: Arc<Mutex<Vec<ModelDelta>>>,
}

impl DeltaRecorder {
    fn new() -> (Self, Arc<Mutex<Vec<ModelDelta>>>) {
        let deltas = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                deltas: deltas.clone(),
            },
            deltas,
        )
    }
}

#[async_trait]
impl Extension for DeltaRecorder {
    fn name(&self) -> &str {
        "delta-recorder"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().model_delta()
    }

    async fn on_model_delta(&self, delta: &ModelDelta) {
        self.deltas.lock().unwrap().push(delta.clone());
    }
}

fn texts(deltas: &[ModelDelta]) -> Vec<String> {
    deltas
        .iter()
        .map(|d| match d {
            ModelDelta::Text { text } => format!("text:{text}"),
            ModelDelta::Reasoning { text } => format!("reasoning:{text}"),
            ModelDelta::ToolInput { text } => format!("tool_input:{text}"),
        })
        .collect()
}

#[tokio::test]
async fn deltas_reach_subscribed_extension_in_order() {
    let model =
        ScriptedModel::new(vec![ModelResponse::final_text("Hello world")]).with_deltas(vec![vec![
            ModelDelta::Reasoning {
                text: "thinking".into(),
            },
            ModelDelta::Text {
                text: "Hello".into(),
            },
            ModelDelta::Text {
                text: " world".into(),
            },
            ModelDelta::ToolInput {
                text: "{\"path\":".into(),
            },
        ]]);
    let (recorder, deltas) = DeltaRecorder::new();

    let agent = Agent::new(model).extension(recorder);
    let answer = timeout(RUN_TIMEOUT, agent.run("hi"))
        .await
        .expect("run timed out")
        .expect("run failed");

    assert_eq!(answer, "Hello world");
    assert_eq!(
        texts(&deltas.lock().unwrap()),
        vec![
            "reasoning:thinking",
            "text:Hello",
            "text: world",
            "tool_input:{\"path\":"
        ]
    );
}

#[tokio::test]
async fn deltas_flow_on_every_model_step() {
    let echo = FnTool::new(
        "echo",
        "echo",
        json!({"type": "object"}),
        |input, _ctx| async move { Ok(input) },
    );
    let model = ScriptedModel::new(vec![
        ModelResponse::tool_calls(vec![call("c1", "echo", json!({"v": 1}))]),
        ModelResponse::final_text("done"),
    ])
    .with_deltas(vec![
        vec![ModelDelta::Text {
            text: "calling".into(),
        }],
        vec![ModelDelta::Text {
            text: "done".into(),
        }],
    ]);
    let (recorder, deltas) = DeltaRecorder::new();

    let agent = Agent::new(model).tool(echo).extension(recorder);
    let answer = timeout(RUN_TIMEOUT, agent.run("go"))
        .await
        .expect("run timed out")
        .expect("run failed");

    assert_eq!(answer, "done");
    assert_eq!(
        texts(&deltas.lock().unwrap()),
        vec!["text:calling", "text:done"]
    );
}

#[tokio::test]
async fn without_delta_subscribers_the_loop_uses_plain_generate() {
    let model = Arc::new(
        ScriptedModel::new(vec![ModelResponse::final_text("ok")])
            .with_deltas(vec![vec![ModelDelta::Text { text: "ok".into() }]]),
    );

    let agent = Agent::new(model.clone());
    let answer = timeout(RUN_TIMEOUT, agent.run("hi"))
        .await
        .expect("run timed out")
        .expect("run failed");

    assert_eq!(answer, "ok");
    assert_eq!(model.generate_calls(), 1);
    assert_eq!(model.streaming_calls(), 0);
}

#[tokio::test]
async fn with_delta_subscribers_the_loop_uses_generate_streaming() {
    let model = Arc::new(
        ScriptedModel::new(vec![ModelResponse::final_text("ok")])
            .with_deltas(vec![vec![ModelDelta::Text { text: "ok".into() }]]),
    );
    let (recorder, deltas) = DeltaRecorder::new();

    let agent = Agent::new(model.clone()).extension(recorder);
    timeout(RUN_TIMEOUT, agent.run("hi"))
        .await
        .expect("run timed out")
        .expect("run failed");

    assert_eq!(model.streaming_calls(), 1);
    assert_eq!(model.generate_calls(), 0);
    assert_eq!(deltas.lock().unwrap().len(), 1);
}

/// A model that only implements `generate`; the default
/// `generate_streaming` must fall back to it.
struct PlainModel;

#[async_trait]
impl Model for PlainModel {
    async fn generate(
        &self,
        _context: &Context,
        _tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        Ok(ModelResponse::final_text("plain"))
    }
}

#[tokio::test]
async fn default_generate_streaming_falls_back_to_generate() {
    let (recorder, deltas) = DeltaRecorder::new();

    let agent = Agent::new(PlainModel).extension(recorder);
    let answer = timeout(RUN_TIMEOUT, agent.run("hi"))
        .await
        .expect("run timed out")
        .expect("run failed");

    assert_eq!(answer, "plain");
    assert!(deltas.lock().unwrap().is_empty());
}

/// An extension subscribed to something else must never see deltas.
struct OtherSubscriber {
    saw_delta: Arc<Mutex<bool>>,
}

#[async_trait]
impl Extension for OtherSubscriber {
    fn name(&self) -> &str {
        "other"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().on_agent_end()
    }

    async fn on_model_delta(&self, _delta: &ModelDelta) {
        *self.saw_delta.lock().unwrap() = true;
    }
}

#[tokio::test]
async fn unsubscribed_extensions_receive_no_deltas() {
    let model = ScriptedModel::new(vec![ModelResponse::final_text("ok")])
        .with_deltas(vec![vec![ModelDelta::Text { text: "ok".into() }]]);
    let saw_delta = Arc::new(Mutex::new(false));
    let (recorder, _deltas) = DeltaRecorder::new();

    let agent = Agent::new(model)
        .extension(OtherSubscriber {
            saw_delta: saw_delta.clone(),
        })
        .extension(recorder);
    timeout(RUN_TIMEOUT, agent.run("hi"))
        .await
        .expect("run timed out")
        .expect("run failed");

    assert!(!*saw_delta.lock().unwrap());
}
