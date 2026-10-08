//! Streaming support: SSE framing and chunk accumulation for the
//! chat-completions streaming protocol. Pure state machines, no I/O —
//! `generate_streaming` in `lib.rs` feeds them from the response body.

use orca_harness_core::{ModelDelta, ModelError, ModelResponse, ToolCall, Usage};
use serde::Deserialize;

use super::{parse_tool_arguments, WireUsage};

#[cfg(test)]
use crate::sse::SseLineBuffer;

#[derive(Deserialize)]
struct WireChunk {
    #[serde(default)]
    id: Option<String>,
    error: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "null_as_empty")]
    choices: Vec<WireChunkChoice>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireChunkChoice {
    delta: WireDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct WireDelta {
    content: Option<String>,
    /// Reasoning channel; compat servers use either name.
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    #[serde(default, deserialize_with = "null_as_empty")]
    tool_calls: Vec<WireToolCallDelta>,
}

/// Some compatible servers send `null` for an empty list (notably
/// `"choices": null` in an error chunk). Treat it as empty so the chunk
/// still parses and its `error` reaches the caller.
fn null_as_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct WireToolCallDelta {
    index: usize,
    id: Option<String>,
    #[serde(default)]
    function: WireFunctionDelta,
}

#[derive(Deserialize, Default)]
struct WireFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Accumulates streamed chunks into the authoritative `ModelResponse`,
/// handing back the render-only deltas each chunk produced.
#[derive(Default)]
pub(crate) struct ChunkAccumulator {
    text: String,
    tool_calls: Vec<PartialToolCall>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
    response_id: Option<String>,
}

impl ChunkAccumulator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The response `id`, from the first chunk that carried one.
    pub(crate) fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    /// Apply one `data:` payload (already stripped of SSE framing, not
    /// `[DONE]`). Returns the deltas this chunk contributes.
    pub(crate) fn apply(&mut self, payload: &str) -> Result<Vec<ModelDelta>, ModelError> {
        let chunk: WireChunk = serde_json::from_str(payload).map_err(|error| {
            ModelError::InvalidResponse(format!(
                "bad stream chunk ({} bytes): {error}",
                payload.len()
            ))
        })?;

        if self.response_id.is_none() {
            self.response_id = chunk.id.filter(|id| !id.is_empty());
        }

        if let Some(error) = chunk.error {
            return Err(crate::http_error::stream_error(&error));
        }

        if let Some(usage) = chunk.usage {
            self.usage = Some(usage.into_usage());
        }

        let mut deltas = Vec::new();
        for choice in chunk.choices {
            if choice.finish_reason.is_some() {
                self.finish_reason = choice.finish_reason;
            }
            let delta = choice.delta;
            for text in [delta.reasoning, delta.reasoning_content]
                .into_iter()
                .flatten()
            {
                if !text.is_empty() {
                    deltas.push(ModelDelta::Reasoning { text });
                }
            }
            if let Some(text) = delta.content {
                if !text.is_empty() {
                    self.text.push_str(&text);
                    deltas.push(ModelDelta::Text { text });
                }
            }
            for fragment in delta.tool_calls {
                if self.tool_calls.len() <= fragment.index {
                    self.tool_calls
                        .resize_with(fragment.index + 1, PartialToolCall::default);
                }
                let partial = &mut self.tool_calls[fragment.index];
                if let Some(id) = fragment.id {
                    partial.id.push_str(&id);
                }
                if let Some(name) = fragment.function.name {
                    partial.name.push_str(&name);
                }
                if let Some(arguments) = fragment.function.arguments {
                    partial.arguments.push_str(&arguments);
                    if !arguments.is_empty() {
                        deltas.push(ModelDelta::ToolInput { text: arguments });
                    }
                }
            }
        }
        Ok(deltas)
    }

    pub(crate) fn finish(self, done_observed: bool) -> Result<ModelResponse, ModelError> {
        let tool_diagnostic = self.tool_diagnostic();
        match self.finish_reason.as_deref() {
            Some("length") => {
                return Err(ModelError::OutputLimit {
                    message: format!(
                        "model output ended while generating{tool_diagnostic}; retry with a smaller payload"
                    ),
                    usage: self.usage,
                });
            }
            Some("content_filter") => {
                return Err(ModelError::ContentFiltered {
                    message: format!("provider stopped generation{tool_diagnostic}"),
                    usage: self.usage,
                });
            }
            _ => {}
        }
        if !done_observed {
            return Err(ModelError::IncompleteResponse {
                message: format!(
                    "stream ended before [DONE]{tool_diagnostic}; retry with a smaller payload"
                ),
                usage: self.usage,
            });
        }

        // A gateway can occasionally close a syntactically complete stream
        // without returning content or a tool call. Treat that as incomplete
        // rather than a successful blank answer so RetryModel can try another
        // routed provider.
        if self.text.is_empty() && self.tool_calls.is_empty() {
            return Err(ModelError::IncompleteResponse {
                message: "stream completed without content or tool calls".into(),
                usage: self.usage,
            });
        }

        if self.tool_calls.is_empty() {
            return Ok(ModelResponse::Final {
                text: self.text,
                usage: self.usage,
            });
        }

        let calls = self
            .tool_calls
            .into_iter()
            .enumerate()
            .map(|(index, partial)| {
                parse_tool_arguments(
                    &partial.name,
                    &partial.arguments,
                    self.finish_reason.as_deref(),
                    self.usage,
                )
                .map(|arguments| ToolCall {
                    id: if partial.id.is_empty() {
                        format!("call_{index}")
                    } else {
                        partial.id
                    },
                    name: partial.name,
                    arguments,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ModelResponse::ToolCalls {
            content: if self.text.is_empty() {
                None
            } else {
                Some(self.text)
            },
            calls,
            usage: self.usage,
        })
    }

    fn tool_diagnostic(&self) -> String {
        self.tool_calls
            .last()
            .map(|call| {
                format!(
                    " tool {} arguments ({} bytes)",
                    if call.name.is_empty() {
                        "<unknown>"
                    } else {
                        &call.name
                    },
                    call.arguments.len()
                )
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests;
