//! A model that records the tool schemas it is offered on every
//! `generate` call, for suites that assert on what a run registers
//! (skills, MCP). Kept out of `common` so that module stays std-only for
//! `consumer_api.rs`.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use orca_harness_core::{Context, Message, Model, ModelError, ModelResponse, ToolCall, ToolSchema};
use serde_json::Value;

/// One scripted model step: a final answer, or one call of the named
/// tool with the given input.
pub enum Step {
    Final,
    Call(&'static str, Value),
}

/// Records the full tool schemas offered on every `generate` call and
/// answers from a script (final text once the script runs out).
#[derive(Default)]
pub struct Recording {
    /// One entry per model call: every schema offered, as JSON, so a
    /// schema whose description or parameters change is not equal to
    /// its former self.
    offered: Mutex<Vec<Vec<Value>>>,
    script: Mutex<VecDeque<Step>>,
}

impl Recording {
    pub fn script(&self, steps: Vec<Step>) {
        *self.script.lock().unwrap() = steps.into();
    }

    /// The schemas of every model call so far, then forgets them.
    pub fn take(&self) -> Vec<Vec<Value>> {
        std::mem::take(&mut *self.offered.lock().unwrap())
    }

    /// The schema names of every model call so far, then forgets them.
    pub fn take_names(&self) -> Vec<Vec<String>> {
        self.take()
            .into_iter()
            .map(|schemas| schemas.iter().map(name).collect())
            .collect()
    }
}

/// The tool name of one recorded schema.
pub fn name(schema: &Value) -> String {
    schema["name"].as_str().unwrap_or_default().to_string()
}

/// Whether one recorded model call offered the tool named `wanted`.
pub fn offers(schemas: &[Value], wanted: &str) -> bool {
    schemas.iter().any(|schema| name(schema) == wanted)
}

/// Every tool result in a transcript, in order, as
/// `(tool name, is_error, output)`.
pub fn tool_results(messages: &[Message]) -> Vec<(String, bool, Value)> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Tool { results } => Some(results.iter().map(|result| {
                (
                    result.tool_name.clone(),
                    result.is_error,
                    result.output.clone(),
                )
            })),
            _ => None,
        })
        .flatten()
        .collect()
}

#[async_trait]
impl Model for Recording {
    async fn generate(
        &self,
        _context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.offered.lock().unwrap().push(
            tools
                .iter()
                .map(|schema| serde_json::to_value(schema).expect("a schema serializes"))
                .collect(),
        );
        let step = self.script.lock().unwrap().pop_front();
        Ok(match step {
            Some(Step::Call(name, arguments)) => ModelResponse::ToolCalls {
                content: None,
                calls: vec![ToolCall {
                    id: format!("call-{name}"),
                    name: name.into(),
                    arguments,
                }],
                usage: None,
            },
            Some(Step::Final) | None => ModelResponse::final_text("done"),
        })
    }
}
