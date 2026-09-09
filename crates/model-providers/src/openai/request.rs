//! Chat-completions request encoding, separate from transport and response handling.
use super::OpenAiModel;
use orca_harness_core::{Context, Message, ToolSchema};
use serde_json::{json, Value};

impl OpenAiModel {
    pub(super) fn request_body(&self, context: &Context, tools: &[ToolSchema]) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": [],
        });
        body["messages"] = Value::Array(encode_messages(context));
        if !tools.is_empty() {
            body["tools"] = Value::Array(
                tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.parameters,
                            }
                        })
                    })
                    .collect(),
            );
            if let Some(enabled) = self.parallel_tool_calls {
                body["parallel_tool_calls"] = json!(enabled);
            }
        }
        if let Some(temperature) = self.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(max_tokens) = self.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        if let Some(effort) = &self.reasoning_effort {
            if self.nested_reasoning {
                body["reasoning"] = json!({"effort": effort});
            } else {
                body["reasoning_effort"] = json!(effort);
            }
        }
        if self.usage_accounting {
            body["usage"] = json!({"include": true});
        }
        if self.prompt_cache {
            body["cache_control"] = json!({"type": "ephemeral"});
        }
        if let Some(session_id) = &self.session_id {
            body["session_id"] = json!(session_id);
        }
        if let Some(prompt_cache_key) = &self.prompt_cache_key {
            body["prompt_cache_key"] = json!(prompt_cache_key);
        }
        body
    }
}

pub(super) fn encode_messages(context: &Context) -> Vec<Value> {
    let mut out = Vec::with_capacity(context.messages().len());
    for message in context.messages() {
        match message {
            Message::System { content } => {
                out.push(json!({"role": "system", "content": content}));
            }
            Message::User { content, images } => {
                if images.is_empty() {
                    out.push(json!({"role": "user", "content": content}));
                } else {
                    let mut parts = vec![json!({"type": "text", "text": content})];
                    parts.extend(images.iter().map(|image| {
                        let mut part = json!({"type": "image_url", "image_url": {}});
                        part["image_url"]["url"] = Value::String(crate::image_data_url(image));
                        part
                    }));
                    let mut message = json!({"role": "user", "content": null});
                    message["content"] = Value::Array(parts);
                    out.push(message);
                }
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut m = json!({"role": "assistant"});
                m["content"] = match content {
                    Some(text) => json!(text),
                    None => Value::Null,
                };
                if !tool_calls.is_empty() {
                    m["tool_calls"] = Value::Array(
                        tool_calls
                            .iter()
                            .map(|c| {
                                let mut call = json!({
                                    "id": c.id,
                                    "type": "function",
                                    "function": {"name": c.name}
                                });
                                // Move the encoded string instead of serializing
                                // it into another owned copy through json!.
                                call["function"]["arguments"] =
                                    Value::String(c.arguments.to_string());
                                call
                            })
                            .collect(),
                    );
                }
                out.push(m);
            }
            Message::Tool { results } => {
                for result in results {
                    let mut message = json!({"role": "tool", "tool_call_id": result.call_id});
                    message["content"] = Value::String(result.output.to_string());
                    out.push(message);
                }
            }
        }
    }
    out
}
