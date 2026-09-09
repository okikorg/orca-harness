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
    error: Option<serde_json::Value>,
    #[serde(default)]
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
    #[serde(default)]
    tool_calls: Vec<WireToolCallDelta>,
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
}

impl ChunkAccumulator {
    pub(crate) fn new() -> Self {
        Self::default()
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
mod tests {
    use super::*;

    #[test]
    fn stream_errors_preserve_provider_retry_policy() {
        for (error, retryable) in [
            (
                serde_json::json!({"code": "insufficient_quota", "message": "quota"}),
                false,
            ),
            (
                serde_json::json!({"code": 403, "message": "forbidden"}),
                false,
            ),
            (
                serde_json::json!({"code": "rate_limit_exceeded", "message": "busy"}),
                true,
            ),
        ] {
            let payload = serde_json::json!({"error": error}).to_string();
            let failure = ChunkAccumulator::new().apply(&payload).unwrap_err();
            assert_eq!(
                crate::http_error::retry_delay(&failure).is_some(),
                retryable
            );
        }
    }

    fn apply_all(acc: &mut ChunkAccumulator, payloads: &[&str]) -> Vec<ModelDelta> {
        payloads
            .iter()
            .flat_map(|p| acc.apply(p).expect("chunk applies"))
            .collect()
    }

    fn delta_tags(deltas: &[ModelDelta]) -> Vec<String> {
        deltas
            .iter()
            .map(|d| match d {
                ModelDelta::Text { text } => format!("text:{text}"),
                ModelDelta::Reasoning { text } => format!("reasoning:{text}"),
                ModelDelta::ToolInput { text } => format!("tool_input:{text}"),
            })
            .collect()
    }

    #[test]
    fn sse_buffer_reassembles_payloads_split_at_arbitrary_boundaries() {
        let mut buf = SseLineBuffer::default();
        assert!(buf.push(b"data: {\"a\":").is_empty());
        let payloads = buf.push(b"1}\r\n\r\ndata: [DONE]\n\n");
        assert_eq!(
            payloads,
            vec!["{\"a\":1}".to_string(), "[DONE]".to_string()]
        );
    }

    #[test]
    fn sse_buffer_consumes_final_frame_without_newline() {
        let mut buf = SseLineBuffer::default();
        assert!(buf.push(b"data: [DONE]").is_empty());
        assert_eq!(buf.finish(), vec!["[DONE]".to_string()]);
    }

    #[test]
    fn sse_buffer_preserves_unicode_at_every_network_split() {
        let payload = r#"{"choices":[{"delta":{"content":"café東京𝄞"}}]}"#;
        for ending in ["\r\n\r\n", ""] {
            let frame = format!("data: {payload}{ending}");
            for split in 0..=frame.len() {
                let mut buffer = SseLineBuffer::default();
                let mut actual = buffer.push(&frame.as_bytes()[..split]);
                actual.extend(buffer.push(&frame.as_bytes()[split..]));
                actual.extend(buffer.finish());
                assert_eq!(actual, [payload], "network split at byte {split}");
            }
        }
    }

    #[test]
    fn partial_tool_json_with_length_is_output_limit_and_keeps_usage() {
        let mut acc = ChunkAccumulator::new();
        apply_all(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"write_file","arguments":"{\"content\":\"unfinished"}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens":12,"completion_tokens":34}}"#,
            ],
        );

        let error = acc.finish(true).unwrap_err();
        match error {
            ModelError::OutputLimit { message, usage } => {
                assert!(message.contains("write_file arguments (22 bytes)"));
                let usage = usage.expect("failed-turn usage retained");
                assert_eq!(usage.input_tokens, 12);
                assert_eq!(usage.output_tokens, 34);
            }
            other => panic!("expected OutputLimit, got {other:?}"),
        }
    }

    #[test]
    fn missing_done_is_incomplete_even_with_valid_terminal_choice() {
        let mut acc = ChunkAccumulator::new();
        apply_all(
            &mut acc,
            &[r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}"#],
        );
        assert!(matches!(
            acc.finish(false),
            Err(ModelError::IncompleteResponse { .. })
        ));
    }

    #[test]
    fn content_filter_is_distinct_and_not_parsed_as_tool_json() {
        let mut acc = ChunkAccumulator::new();
        apply_all(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"write_file","arguments":"{\"content\":"}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"content_filter"}]}"#,
            ],
        );
        assert!(matches!(
            acc.finish(true),
            Err(ModelError::ContentFiltered { .. })
        ));
    }

    #[test]
    fn malformed_tool_json_after_tool_calls_is_not_classified_as_truncated() {
        let mut acc = ChunkAccumulator::new();
        apply_all(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"shell","arguments":"{not-json}"}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ],
        );
        assert!(matches!(
            acc.finish(true),
            Err(ModelError::MalformedToolArguments {
                ref tool_name,
                argument_bytes: 10,
                ..
            }) if tool_name == "shell"
        ));
    }

    #[test]
    fn text_stream_emits_deltas_and_accumulates_final_text() {
        let mut acc = ChunkAccumulator::new();
        let deltas = apply_all(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ],
        );
        assert_eq!(delta_tags(&deltas), vec!["text:Hel", "text:lo"]);
        match acc.finish(true).unwrap() {
            ModelResponse::Final { text, usage } => {
                assert_eq!(text, "Hello");
                assert!(usage.is_none());
            }
            other => panic!("expected Final, got {other:?}"),
        }
    }

    #[test]
    fn completed_empty_stream_is_retryable_instead_of_a_blank_answer() {
        let mut acc = ChunkAccumulator::new();
        apply_all(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":0}}"#,
            ],
        );

        match acc.finish(true) {
            Err(ModelError::IncompleteResponse { message, usage }) => {
                assert!(message.contains("without content or tool calls"));
                assert_eq!(usage.expect("usage retained").output_tokens, 0);
            }
            other => panic!("expected retryable incomplete response, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_deltas_pass_through_but_stay_out_of_final_text() {
        let mut acc = ChunkAccumulator::new();
        let deltas = apply_all(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"reasoning":"let me think"},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{"reasoning_content":" more"},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{"content":"answer"},"finish_reason":null}]}"#,
            ],
        );
        assert_eq!(
            delta_tags(&deltas),
            vec!["reasoning:let me think", "reasoning: more", "text:answer"]
        );
        match acc.finish(true).unwrap() {
            ModelResponse::Final { text, .. } => assert_eq!(text, "answer"),
            other => panic!("expected Final, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_arguments_assemble_across_chunks() {
        let mut acc = ChunkAccumulator::new();
        let deltas = apply_all(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"shell","arguments":""}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":"}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ],
        );
        assert_eq!(
            delta_tags(&deltas),
            vec!["tool_input:{\"command\":", "tool_input:\"ls\"}"]
        );
        match acc.finish(true).unwrap() {
            ModelResponse::ToolCalls { content, calls, .. } => {
                assert!(content.is_none());
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_1");
                assert_eq!(calls[0].name, "shell");
                assert_eq!(calls[0].arguments["command"], "ls");
            }
            other => panic!("expected ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn multiple_tool_calls_merge_by_index_and_empty_args_become_object() {
        let mut acc = ChunkAccumulator::new();
        apply_all(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"grep"}},{"index":1,"id":"b","function":{"name":"list_dir","arguments":""}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pattern\":\"x\"}"}}]},"finish_reason":null}]}"#,
            ],
        );
        match acc.finish(true).unwrap() {
            ModelResponse::ToolCalls { calls, .. } => {
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].name, "grep");
                assert_eq!(calls[0].arguments["pattern"], "x");
                assert_eq!(calls[1].name, "list_dir");
                assert!(calls[1].arguments.as_object().unwrap().is_empty());
            }
            other => panic!("expected ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn openrouter_cache_reads_and_writes_are_disjoint() {
        // OpenRouter usage accounting: cached_tokens (reads) and
        // cache_write_tokens are separate, non-overlapping counts, not
        // reads-plus-writes folded into one field. Numbers below are a
        // real captured OpenRouter/Bedrock response for a request that
        // both read an existing cache prefix and wrote a new one:
        // prompt_tokens=19848, cached_tokens=9923, cache_write_tokens=9913
        // (see #19). The two sum to prompt_tokens minus a small genuine
        // uncached remainder; they must not be subtracted from each other.
        let mut acc = ChunkAccumulator::new();
        let _ = apply_all(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":19848,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":9923,"cache_write_tokens":9913}}}"#,
            ],
        );
        match acc.finish(true).unwrap() {
            ModelResponse::Final { usage, .. } => {
                let usage = usage.expect("usage captured");
                assert_eq!(usage.cache_read_tokens, 9923);
                assert_eq!(usage.cache_create_tokens, 9913);
                assert_eq!(usage.input_tokens, 12, "19848 minus read minus write");
                assert_eq!(usage.context_tokens(), 19848 + 5);
            }
            other => panic!("expected Final, got {other:?}"),
        }
    }

    #[test]
    fn usage_only_chunk_with_empty_choices_is_captured() {
        let mut acc = ChunkAccumulator::new();
        apply_all(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":34,"prompt_tokens_details":{"cached_tokens":5}}}"#,
            ],
        );
        match acc.finish(true).unwrap() {
            ModelResponse::Final { usage, .. } => {
                let usage = usage.expect("usage captured");
                // prompt_tokens (12) includes the 5 cached: normalized
                // input is the uncached remainder.
                assert_eq!(usage.input_tokens, 7);
                assert_eq!(usage.output_tokens, 34);
                assert_eq!(usage.cache_read_tokens, 5);
                assert_eq!(usage.context_tokens(), 46);
            }
            other => panic!("expected Final, got {other:?}"),
        }
    }

    #[test]
    fn accompanying_text_survives_alongside_tool_calls() {
        let mut acc = ChunkAccumulator::new();
        apply_all(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"content":"Running ls."},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"shell","arguments":"{}"}}]},"finish_reason":null}]}"#,
            ],
        );
        match acc.finish(true).unwrap() {
            ModelResponse::ToolCalls { content, .. } => {
                assert_eq!(content.as_deref(), Some("Running ls."));
            }
            other => panic!("expected ToolCalls, got {other:?}"),
        }
    }
}
