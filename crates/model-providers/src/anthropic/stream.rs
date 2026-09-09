use std::collections::BTreeMap;
use std::sync::Arc;

use orca_harness_core::{ModelDelta, ModelError, ModelResponse, ToolCall, Usage};
use serde_json::Value;

#[derive(Default)]
pub(super) struct Accumulator {
    started: bool,
    stopped: bool,
    stop_reason: Option<String>,
    usage: Option<Usage>,
    blocks: BTreeMap<u64, Block>,
}

/// A finished response plus the signed thinking blocks that produced it.
pub(super) struct Collected {
    pub(super) response: ModelResponse,
    pub(super) thinking: Arc<[Value]>,
}

struct Block {
    value: Value,
    arguments: String,
    closed: bool,
}

fn invalid(message: &str) -> ModelError {
    ModelError::InvalidResponse(format!("Anthropic: {message}"))
}

impl Accumulator {
    pub(super) fn stopped(&self) -> bool {
        self.stopped
    }

    pub(super) fn apply(&mut self, payload: &str) -> Result<Vec<ModelDelta>, ModelError> {
        let event: Value =
            serde_json::from_str(payload).map_err(|error| invalid(&error.to_string()))?;
        let mut deltas = Vec::new();
        match event["type"].as_str() {
            Some("error") => return Err(crate::http_error::stream_error(&event["error"])),
            Some("message_start") => {
                if self.started {
                    return Err(invalid("duplicate message_start"));
                }
                self.started = true;
                self.update_usage(&event["message"]["usage"]);
            }
            Some("content_block_start") => {
                if !self.started {
                    return Err(invalid("content before message_start"));
                }
                let index = event["index"]
                    .as_u64()
                    .ok_or_else(|| invalid("missing block index"))?;
                let value = event["content_block"].clone();
                if let Some(text) = value["text"].as_str().filter(|text| !text.is_empty()) {
                    deltas.push(ModelDelta::Text { text: text.into() });
                }
                if let Some(text) = value["thinking"].as_str().filter(|text| !text.is_empty()) {
                    deltas.push(ModelDelta::Reasoning { text: text.into() });
                }
                if self
                    .blocks
                    .insert(
                        index,
                        Block {
                            value,
                            arguments: String::new(),
                            closed: false,
                        },
                    )
                    .is_some()
                {
                    return Err(invalid("duplicate content block"));
                }
            }
            Some("content_block_delta" | "content_block_stop") => {
                let index = event["index"]
                    .as_u64()
                    .ok_or_else(|| invalid("missing block index"))?;
                let block = self
                    .blocks
                    .get_mut(&index)
                    .filter(|block| !block.closed)
                    .ok_or_else(|| invalid("delta/stop for unknown or closed block"))?;
                if event["type"] == "content_block_stop" {
                    block.closed = true;
                } else {
                    let delta = &event["delta"];
                    match delta["type"].as_str() {
                        Some("text_delta") => {
                            if block.value["type"] != "text" {
                                return Err(invalid("text delta for non-text block"));
                            }
                            let text = delta["text"]
                                .as_str()
                                .ok_or_else(|| invalid("missing text delta"))?;
                            let mut content =
                                block.value["text"].as_str().unwrap_or_default().to_owned();
                            content.push_str(text);
                            block.value["text"] = Value::String(content);
                            deltas.push(ModelDelta::Text { text: text.into() });
                        }
                        Some("input_json_delta") => {
                            if block.value["type"] != "tool_use" {
                                return Err(invalid("input delta for non-tool block"));
                            }
                            let text = delta["partial_json"]
                                .as_str()
                                .ok_or_else(|| invalid("missing input delta"))?;
                            block.arguments.push_str(text);
                            deltas.push(ModelDelta::ToolInput { text: text.into() });
                        }
                        // Thinking text and its signature both arrive by delta;
                        // the signature must be replayed byte-for-byte, so
                        // accumulate it onto the block rather than dropping it.
                        Some(kind @ ("thinking_delta" | "signature_delta")) => {
                            if block.value["type"] != "thinking" {
                                return Err(invalid("thinking delta for non-thinking block"));
                            }
                            let field = kind.trim_end_matches("_delta");
                            let text = delta[field]
                                .as_str()
                                .ok_or_else(|| invalid("missing thinking delta"))?;
                            let mut content =
                                block.value[field].as_str().unwrap_or_default().to_owned();
                            content.push_str(text);
                            block.value[field] = Value::String(content);
                            if field == "thinking" {
                                deltas.push(ModelDelta::Reasoning { text: text.into() });
                            }
                        }
                        // New delta/event types are forward-compatible. Unknown
                        // content blocks are rejected at finish, never executed.
                        _ => {}
                    }
                }
            }
            Some("message_delta") => {
                if !self.started {
                    return Err(invalid("message_delta before message_start"));
                }
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.into());
                }
                self.update_usage(&event["usage"]);
            }
            Some("message_stop") => self.stopped = true,
            Some(_) => {}
            None => return Err(invalid("missing event type")),
        }
        Ok(deltas)
    }

    fn update_usage(&mut self, value: &Value) {
        if !value.is_object() {
            return;
        }
        let usage = self.usage.get_or_insert_with(Usage::default);
        // Start supplies input/cache counts; deltas supply cumulative output
        // counts and may repeat other fields. Replace only fields present.
        for (field, target) in [
            ("input_tokens", &mut usage.input_tokens),
            ("output_tokens", &mut usage.output_tokens),
            ("cache_read_input_tokens", &mut usage.cache_read_tokens),
            (
                "cache_creation_input_tokens",
                &mut usage.cache_create_tokens,
            ),
        ] {
            if let Some(count) = value[field].as_u64() {
                *target = count;
            }
        }
    }

    pub(super) fn finish(self) -> Result<Collected, ModelError> {
        match self.stop_reason.as_deref() {
            Some("max_tokens" | "model_context_window_exceeded") => {
                return Err(ModelError::OutputLimit {
                    message: "Anthropic reached its token limit".into(),
                    usage: self.usage,
                })
            }
            Some("refusal") => {
                return Err(ModelError::ContentFiltered {
                    message: "Anthropic refused the response".into(),
                    usage: self.usage,
                })
            }
            _ => {}
        }
        if !self.started || !self.stopped || self.blocks.values().any(|block| !block.closed) {
            return Err(ModelError::IncompleteResponse {
                message: "Anthropic stream ended before all blocks and message_stop".into(),
                usage: self.usage,
            });
        }
        if !matches!(
            self.stop_reason.as_deref(),
            Some("end_turn" | "stop_sequence" | "tool_use")
        ) {
            return Err(ModelError::IncompleteResponse {
                message: format!("unsupported Anthropic stop reason: {:?}", self.stop_reason),
                usage: self.usage,
            });
        }
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut thinking = Vec::new();
        for block in self.blocks.into_values() {
            match block.value["type"].as_str() {
                Some("text") => text.push_str(
                    block.value["text"]
                        .as_str()
                        .ok_or_else(|| invalid("missing text"))?,
                ),
                Some("tool_use") => {
                    let id = block.value["id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| invalid("missing tool id"))?;
                    let name = block.value["name"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| invalid("missing tool name"))?;
                    let malformed = |message: String| ModelError::MalformedToolArguments {
                        tool_name: name.into(),
                        argument_bytes: block.arguments.len(),
                        finish_reason: self.stop_reason.clone(),
                        message,
                        usage: self.usage,
                    };
                    let arguments: Value = if block.arguments.is_empty() {
                        block.value["input"].clone()
                    } else {
                        serde_json::from_str(&block.arguments)
                            .map_err(|error| malformed(error.to_string()))?
                    };
                    if !arguments.is_object() {
                        return Err(malformed("tool input must be an object".into()));
                    }
                    if calls.iter().any(|call: &ToolCall| call.id == id) {
                        return Err(invalid("duplicate tool id"));
                    }
                    calls.push(ToolCall {
                        id: id.into(),
                        name: name.into(),
                        arguments,
                    });
                }
                // Reasoning is presentation-only and never joins the answer
                // text, but a thinking-on model requires its signed blocks
                // back on the assistant turn that made the tool calls.
                Some("thinking" | "redacted_thinking") => thinking.push(block.value),
                _ => return Err(invalid("unsupported content block")),
            }
        }
        if (self.stop_reason.as_deref() == Some("tool_use")) != !calls.is_empty() {
            return Err(invalid("stop reason does not match tool calls"));
        }
        let thinking: Arc<[Value]> = thinking.into();
        if !calls.is_empty() {
            Ok(Collected {
                response: ModelResponse::ToolCalls {
                    content: (!text.is_empty()).then_some(text),
                    calls,
                    usage: self.usage,
                },
                thinking,
            })
        } else if !text.is_empty() {
            Ok(Collected {
                response: ModelResponse::Final {
                    text,
                    usage: self.usage,
                },
                thinking,
            })
        } else {
            Err(ModelError::IncompleteResponse {
                message: "Anthropic returned no text or tools".into(),
                usage: self.usage,
            })
        }
    }
}
