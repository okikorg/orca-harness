use orca_harness_core::{ModelDelta, ModelError, ModelResponse, ToolCall, Usage};
use serde_json::Value;

#[derive(Default)]
pub(crate) struct Accumulator {
    text: String,
    calls: Vec<ToolCall>,
    usage: Option<Usage>,
    completed: bool,
    reasoning: Vec<Value>,
}

impl Accumulator {
    pub(crate) fn apply(&mut self, payload: &str) -> Result<Vec<ModelDelta>, ModelError> {
        let event: Value = serde_json::from_str(payload)
            .map_err(|e| ModelError::InvalidResponse(format!("bad Codex event: {e}")))?;
        let kind = event["type"].as_str().unwrap_or_default();
        match kind {
            "response.output_text.delta" => {
                let text = event["delta"].as_str().unwrap_or_default().to_string();
                self.text.push_str(&text);
                Ok(if text.is_empty() {
                    vec![]
                } else {
                    vec![ModelDelta::Text { text }]
                })
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let text = event["delta"].as_str().unwrap_or_default().to_string();
                Ok(if text.is_empty() {
                    vec![]
                } else {
                    vec![ModelDelta::Reasoning { text }]
                })
            }
            "response.output_item.done" => {
                if event["item"]["type"] == "reasoning" {
                    let item = &event["item"];
                    let encrypted = item["encrypted_content"].as_str().ok_or_else(|| {
                        ModelError::InvalidResponse(
                            "reasoning item missing encrypted_content".into(),
                        )
                    })?;
                    self.reasoning.push(serde_json::json!({
                        "type": "reasoning", "id": item["id"],
                        "summary": item["summary"], "encrypted_content": encrypted,
                    }));
                } else if event["item"]["type"] == "function_call" {
                    let item = &event["item"];
                    let call_id = item["call_id"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            ModelError::InvalidResponse("tool call missing call_id".into())
                        })?;
                    let name = item["name"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            ModelError::InvalidResponse("tool call missing name".into())
                        })?;
                    let raw_arguments = item["arguments"].as_str().ok_or_else(|| {
                        ModelError::InvalidResponse("tool call missing arguments".into())
                    })?;
                    let arguments = serde_json::from_str(raw_arguments).map_err(|e| {
                        ModelError::InvalidResponse(format!("bad tool arguments: {e}"))
                    })?;
                    self.calls.push(ToolCall {
                        id: call_id.to_string(),
                        name: name.to_string(),
                        arguments,
                    });
                    return Ok((!raw_arguments.is_empty())
                        .then(|| ModelDelta::ToolInput {
                            text: raw_arguments.to_string(),
                        })
                        .into_iter()
                        .collect());
                }
                Ok(vec![])
            }
            "response.completed" => {
                self.completed = true;
                self.usage = parse_usage(&event["response"]["usage"]);
                Ok(vec![])
            }
            "response.failed" | "response.incomplete" | "error" => {
                Err(crate::http_error::stream_error(
                    event
                        .pointer("/response/error")
                        .or_else(|| event.get("error"))
                        .unwrap_or(&event),
                ))
            }
            _ => Ok(vec![]),
        }
    }

    pub(crate) fn finish(self) -> Result<ModelResponse, ModelError> {
        if !self.completed {
            return Err(ModelError::InvalidResponse(
                "Codex stream ended before response.completed".into(),
            ));
        }
        if self.calls.is_empty() {
            Ok(ModelResponse::Final {
                text: self.text,
                usage: self.usage,
            })
        } else {
            Ok(ModelResponse::ToolCalls {
                content: (!self.text.is_empty()).then_some(self.text),
                calls: self.calls,
                usage: self.usage,
            })
        }
    }

    pub(crate) fn take_reasoning(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.reasoning)
    }
}

fn parse_usage(value: &Value) -> Option<Usage> {
    value.as_object()?;
    let input = value["input_tokens"].as_u64().unwrap_or(0);
    let cached = value
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(Usage {
        input_tokens: input.saturating_sub(cached),
        output_tokens: value["output_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: cached,
        cache_create_tokens: 0,
        reasoning_tokens: value
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseBuffer;

    #[test]
    fn streams_summary_and_preserves_exact_reasoning_usage() {
        let mut acc = Accumulator::default();
        let deltas = acc
            .apply(
                r#"{"type":"response.reasoning_summary_text.delta","delta":"Checking the code."}"#,
            )
            .unwrap();
        assert!(
            matches!(deltas.as_slice(), [ModelDelta::Reasoning { text }] if text == "Checking the code.")
        );
        acc.apply(r#"{"type":"response.output_text.delta","delta":"Done."}"#)
            .unwrap();
        acc.apply(r#"{"type":"response.completed","response":{"usage":{"input_tokens":100,"output_tokens":850,"output_tokens_details":{"reasoning_tokens":600}}}}"#).unwrap();
        let response = acc.finish().unwrap();
        let usage = response.usage().unwrap();
        assert_eq!(usage.reasoning_tokens, Some(600));
        assert_eq!(usage.output_tokens, 850);
        assert_eq!(usage.context_tokens(), 950);
        assert!(matches!(response, ModelResponse::Final { text, .. } if text == "Done."));
        assert_eq!(
            parse_usage(&serde_json::json!({"output_tokens": 3}))
                .unwrap()
                .reasoning_tokens,
            None
        );
        assert_eq!(
            parse_usage(&serde_json::json!({"output_tokens_details": {"reasoning_tokens": 0}}))
                .unwrap()
                .reasoning_tokens,
            Some(0)
        );
    }

    #[test]
    fn stream_errors_preserve_provider_retry_policy() {
        for code in ["insufficient_quota", "rate_limit_exceeded"] {
            for event in [
                serde_json::json!({"type": "response.failed", "response": {"error": {"code": code}}}),
                serde_json::json!({"type": "error", "error": {"code": code}}),
                serde_json::json!({"type": "error", "code": code}),
            ] {
                let error = Accumulator::default()
                    .apply(&event.to_string())
                    .unwrap_err();
                assert_eq!(
                    crate::http_error::retry_delay(&error).is_some(),
                    code == "rate_limit_exceeded"
                );
            }
        }
    }

    #[test]
    fn accumulates_text_tool_and_usage() {
        let mut acc = Accumulator::default();
        acc.apply(r#"{"type":"response.output_text.delta","delta":"hello"}"#)
            .unwrap();
        let deltas = acc.apply(r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"cmd\":\"pwd\"}"}}"#).unwrap();
        assert!(matches!(
            deltas.as_slice(),
            [ModelDelta::ToolInput { text }] if text == r#"{"cmd":"pwd"}"#
        ));
        acc.apply(r#"{"type":"response.completed","response":{"usage":{"input_tokens":12,"output_tokens":3,"input_tokens_details":{"cached_tokens":5}}}}"#).unwrap();
        match acc.finish().unwrap() {
            ModelResponse::ToolCalls {
                content,
                calls,
                usage,
            } => {
                assert_eq!(content.as_deref(), Some("hello"));
                assert_eq!(calls[0].name, "shell");
                assert_eq!(usage.unwrap().input_tokens, 7);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rejects_incomplete_stream() {
        assert!(Accumulator::default().finish().is_err());
    }

    #[test]
    fn sse_buffer_accepts_crlf_and_split_frames() {
        let mut buffer = SseBuffer::default();
        assert!(buffer
            .push(b"event: response.output_text.delta\r\ndata:")
            .unwrap()
            .is_empty());
        assert_eq!(
            buffer
                .push(b" {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\r\n\r\n")
                .unwrap(),
            [r#"{"type":"response.output_text.delta","delta":"x"}"#]
        );
    }

    #[test]
    fn sse_buffer_preserves_utf8_split_at_every_byte() {
        let frame = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"héllo 世界\"}\n\n";
        for split in 0..=frame.len() {
            let mut buffer = SseBuffer::default();
            let mut events = buffer.push(&frame.as_bytes()[..split]).unwrap();
            events.extend(buffer.push(&frame.as_bytes()[split..]).unwrap());
            assert_eq!(events.len(), 1, "split {split}");
            assert!(events[0].contains("héllo 世界"));
        }
    }

    #[test]
    fn sse_buffer_uses_the_earliest_mixed_line_ending_delimiter() {
        let mut buffer = SseBuffer::default();
        let events = buffer
            .push(b"data: {\"id\":1}\n\ndata: {\"id\":2}\r\n\r\n")
            .unwrap();
        assert_eq!(events, [r#"{"id":1}"#, r#"{"id":2}"#]);
    }

    #[test]
    fn incomplete_terminal_event_is_an_error() {
        let error = Accumulator::default()
            .apply(r#"{"type":"response.incomplete","response":{"error":{"message":"limit"}}}"#)
            .unwrap_err();
        assert!(error.to_string().contains("limit"));
    }

    #[test]
    fn captures_encrypted_reasoning_and_rejects_incomplete_tool_calls() {
        let mut accumulator = Accumulator::default();
        accumulator.apply(r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"r1","summary":[],"encrypted_content":"opaque"}}"#).unwrap();
        assert_eq!(
            accumulator.take_reasoning()[0]["encrypted_content"],
            "opaque"
        );
        let error = accumulator.apply(
            r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"shell","arguments":"{}"}}"#,
        ).unwrap_err();
        assert!(error.to_string().contains("call_id"));
    }
}
