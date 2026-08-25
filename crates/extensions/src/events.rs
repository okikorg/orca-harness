//! Event stream extension: turns the kernel lifecycle into a typed stream
//! of [`HarnessEvent`]s delivered to a sink. This is the seam most hosts
//! build on — a sidecar serializes each event to NDJSON, a TUI renders
//! them, a test collects them. Emitting is the extension's whole job; it
//! never mutates context or results.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use orca_harness_core::{
    Context, Extension, ExtensionError, HarnessError, ModelDelta, ModelResponse, Subscriptions,
    ToolCall, ToolDecision, ToolResult, Usage,
};

/// A typed lifecycle event. The tags mirror the platform NDJSON union
/// (`assistant`, `tool_call`, `tool_result`, `usage`, `result`, `error`)
/// so a host can serialize directly, but the type is transport-agnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    /// The run started.
    AgentStart,
    /// An incremental fragment of assistant text, emitted while the model
    /// is still generating. The complete text still arrives as
    /// [`Assistant`](HarnessEvent::Assistant); deltas are render-only.
    AssistantDelta { text: String },
    /// An incremental fragment of the model's reasoning channel.
    /// Ephemeral: reasoning never enters the context or the final result.
    ReasoningDelta { text: String },
    /// An incremental fragment of tool-call arguments, used for live
    /// output-token progress without exposing an incomplete tool call.
    ToolInputDelta { text: String },
    /// The model produced assistant text (with or without tool calls).
    Assistant { message: String },
    /// The model requested a tool call.
    ToolCall {
        tool_call_id: String,
        tool_name: String,
        input: Value,
    },
    /// A tool produced a result.
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        output: Value,
        is_error: bool,
    },
    /// Token usage from a model step.
    Usage { usage: Usage },
    /// The run finished successfully.
    Result { message: String },
    /// The run failed.
    Error { message: String },
}

/// Receives events as they happen. Implemented for `Fn(HarnessEvent)` and
/// for an `mpsc::UnboundedSender<HarnessEvent>` via the provided
/// constructors.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn emit(&self, event: HarnessEvent);
}

#[async_trait]
impl<F> EventSink for F
where
    F: Fn(HarnessEvent) + Send + Sync,
{
    async fn emit(&self, event: HarnessEvent) {
        self(event)
    }
}

/// Streams lifecycle events to a sink. Subscribes only to the hooks it
/// needs, so it stays off the kernel's hot path for everything else.
pub struct EventStream {
    sink: Arc<dyn EventSink>,
}

impl EventStream {
    pub fn new(sink: Arc<dyn EventSink>) -> Self {
        Self { sink }
    }

    /// Build from a plain closure.
    pub fn from_fn<F>(f: F) -> Self
    where
        F: Fn(HarnessEvent) + Send + Sync + 'static,
    {
        Self { sink: Arc::new(f) }
    }

    /// Build a stream plus a channel receiver draining its events.
    pub fn channel() -> (Self, tokio::sync::mpsc::UnboundedReceiver<HarnessEvent>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self::new(Arc::new(ChannelSink(tx))), rx)
    }
}

struct ChannelSink(tokio::sync::mpsc::UnboundedSender<HarnessEvent>);

#[async_trait]
impl EventSink for ChannelSink {
    async fn emit(&self, event: HarnessEvent) {
        let _ = self.0.send(event);
    }
}

#[async_trait]
impl Extension for EventStream {
    fn name(&self) -> &str {
        "event-stream"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none()
            .on_agent_start()
            .model_delta()
            .after_model()
            .before_tool()
            .tool_result()
            .on_error()
    }

    async fn on_model_delta(&self, delta: &ModelDelta) {
        let event = match delta {
            ModelDelta::Text { text } => HarnessEvent::AssistantDelta { text: text.clone() },
            ModelDelta::Reasoning { text } => HarnessEvent::ReasoningDelta { text: text.clone() },
            ModelDelta::ToolInput { text } => HarnessEvent::ToolInputDelta { text: text.clone() },
        };
        self.sink.emit(event).await;
    }

    async fn on_agent_start(&self, _context: &mut Context) -> Result<(), ExtensionError> {
        self.sink.emit(HarnessEvent::AgentStart).await;
        Ok(())
    }

    async fn after_model(
        &self,
        _context: &mut Context,
        response: &ModelResponse,
    ) -> Result<(), ExtensionError> {
        match response {
            ModelResponse::Final { text, usage } => {
                if !text.is_empty() {
                    self.sink
                        .emit(HarnessEvent::Assistant {
                            message: text.clone(),
                        })
                        .await;
                }
                if let Some(usage) = usage {
                    self.sink.emit(HarnessEvent::Usage { usage: *usage }).await;
                }
                self.sink
                    .emit(HarnessEvent::Result {
                        message: text.clone(),
                    })
                    .await;
            }
            ModelResponse::ToolCalls { content, usage, .. } => {
                if let Some(message) = content {
                    self.sink
                        .emit(HarnessEvent::Assistant {
                            message: message.clone(),
                        })
                        .await;
                }
                if let Some(usage) = usage {
                    self.sink.emit(HarnessEvent::Usage { usage: *usage }).await;
                }
            }
        }
        Ok(())
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        self.sink
            .emit(HarnessEvent::ToolCall {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                input: call.arguments.clone(),
            })
            .await;
        Ok(ToolDecision::Continue)
    }

    async fn tool_result(&self, result: &ToolResult) {
        self.sink
            .emit(HarnessEvent::ToolResult {
                tool_call_id: result.call_id.clone(),
                tool_name: result.tool_name.clone(),
                output: result.output.clone(),
                is_error: result.is_error,
            })
            .await;
    }

    async fn on_error(&self, error: &HarnessError) {
        self.sink
            .emit(HarnessEvent::Error {
                message: error.to_string(),
            })
            .await;
    }
}
