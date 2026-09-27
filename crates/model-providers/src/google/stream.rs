use orca_harness_core::{ModelDelta, ModelError, ModelResponse, ToolCall, Usage};
use serde_json::Value;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

static CALL_ID: AtomicU64 = AtomicU64::new(1);
#[derive(Default)]
pub(super) struct Accumulator {
    text: String,
    calls: Vec<ToolCall>,
    parts: Vec<Value>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
    seen: bool,
    prompt_blocked: Option<String>,
}
pub(super) struct Collected {
    pub response: ModelResponse,
    pub parts: Arc<[Value]>,
}
fn invalid(s: impl Into<String>) -> ModelError {
    ModelError::InvalidResponse(format!("Google: {}", s.into()))
}
impl Accumulator {
    pub fn apply(&mut self, payload: &str) -> Result<Vec<ModelDelta>, ModelError> {
        let chunk: Value = serde_json::from_str(payload).map_err(|e| invalid(e.to_string()))?;
        if let Some(error) = chunk.get("error") {
            return Err(crate::http_error::stream_error(error));
        }
        self.seen = true;
        if let Some(meta) = chunk.get("usageMetadata") {
            let usage = self.usage.get_or_insert_with(Usage::default);
            let prompt = meta["promptTokenCount"]
                .as_u64()
                .unwrap_or(usage.input_tokens.saturating_add(usage.cache_read_tokens));
            if let Some(cached) = meta["cachedContentTokenCount"].as_u64() {
                usage.cache_read_tokens = cached;
            }
            usage.input_tokens = prompt.saturating_sub(usage.cache_read_tokens);
            if let Some(thoughts) = meta["thoughtsTokenCount"].as_u64() {
                usage.reasoning_tokens = Some(thoughts);
            }
            let candidates = meta["candidatesTokenCount"].as_u64().unwrap_or_else(|| {
                usage
                    .output_tokens
                    .saturating_sub(usage.reasoning_tokens.unwrap_or(0))
            });
            usage.output_tokens = candidates.saturating_add(usage.reasoning_tokens.unwrap_or(0));
        }
        if let Some(reason) = chunk["promptFeedback"]["blockReason"]
            .as_str()
            .filter(|s| !s.is_empty() && *s != "BLOCK_REASON_UNSPECIFIED")
        {
            self.prompt_blocked = Some(reason.into());
        }
        let mut deltas = Vec::new();
        if let Some(candidate) = chunk["candidates"].as_array().and_then(|a| a.first()) {
            if let Some(reason) = candidate["finishReason"].as_str() {
                self.finish_reason = Some(reason.into());
            }
            if let Some(parts) = candidate["content"]["parts"].as_array() {
                for part in parts {
                    if let Some(call) = part.get("functionCall") {
                        let name = call["name"]
                            .as_str()
                            .filter(|n| !n.is_empty())
                            .ok_or_else(|| invalid("missing function name"))?;
                        let args = call["args"].clone();
                        if !args.is_object() {
                            return Err(ModelError::MalformedToolArguments {
                                tool_name: name.into(),
                                argument_bytes: args.to_string().len(),
                                finish_reason: self.finish_reason.clone(),
                                message: "function args must be an object".into(),
                                usage: self.usage,
                            });
                        }
                        deltas.push(ModelDelta::ToolInput {
                            text: args.to_string(),
                        });
                        self.calls.push(ToolCall {
                            id: call["id"]
                                .as_str()
                                .filter(|id| !id.is_empty())
                                .map(str::to_owned)
                                .unwrap_or_else(|| {
                                    format!("google-{}", CALL_ID.fetch_add(1, Ordering::Relaxed))
                                }),
                            name: name.into(),
                            arguments: args,
                        });
                        self.parts.push(part.clone());
                    } else if let Some(text) = part["text"].as_str() {
                        if part["thought"] == true {
                            deltas.push(ModelDelta::Reasoning { text: text.into() });
                            self.parts.push(part.clone());
                        } else {
                            self.text.push_str(text);
                            deltas.push(ModelDelta::Text { text: text.into() });
                        }
                    }
                }
            }
        }
        Ok(deltas)
    }
    pub fn finish(self) -> Result<Collected, ModelError> {
        if let Some(reason) = &self.prompt_blocked {
            return Err(ModelError::ContentFiltered {
                message: format!("Google blocked prompt: {reason}"),
                usage: self.usage,
            });
        }
        match self.finish_reason.as_deref() {
            Some("MAX_TOKENS") => {
                return Err(ModelError::OutputLimit {
                    message: "Google reached its token limit".into(),
                    usage: self.usage,
                })
            }
            Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII") => {
                return Err(ModelError::ContentFiltered {
                    message: format!("Google stopped: {:?}", self.finish_reason),
                    usage: self.usage,
                })
            }
            Some("STOP") => {}
            _ => {
                return Err(ModelError::IncompleteResponse {
                    message: format!("Google stream ended without STOP: {:?}", self.finish_reason),
                    usage: self.usage,
                })
            }
        }
        if !self.seen || (self.calls.is_empty() && self.text.is_empty()) {
            return Err(ModelError::IncompleteResponse {
                message: "Google returned no content".into(),
                usage: self.usage,
            });
        }
        let response = crate::response(self.text, self.calls, self.usage);
        Ok(Collected {
            response,
            parts: self.parts.into(),
        })
    }
}
