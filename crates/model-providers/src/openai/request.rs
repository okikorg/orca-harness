//! Chat-completions request encoding, separate from transport and response handling.
use super::OpenAiModel;
use orca_harness_core::{Context, Message, ModelError, ToolSchema};
use serde_json::{Value, json};

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
                                // Gateways like OpenRouter re-encode this for
                                // Anthropic, which rejects top-level combinators.
                                "parameters": crate::anthropic::input_schema(&t.parameters),
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
        if let Some(tier) = &self.service_tier {
            body["service_tier"] = json!(tier);
        }
        if let Some(verbosity) = &self.verbosity {
            body["verbosity"] = json!(verbosity);
        }
        if let Some(format) = &self.response_format {
            body["response_format"] = format.clone();
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
                        part["image_url"]["url"] = Value::String(crate::image_url(image));
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

/// Responses' JSON-schema format is flat; Chat wraps schema attributes.
pub(super) fn chat_response_format(format: Value) -> Result<Value, ModelError> {
    let invalid = |message: &str| ModelError::Request(format!("invalid text.format: {message}"));
    let Value::Object(mut fields) = format else {
        return Err(invalid("expected an object"));
    };
    let kind = fields
        .remove("type")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| invalid("type must be a string"))?;
    match kind.as_str() {
        "text" | "json_object" if fields.is_empty() => Ok(json!({"type":kind})),
        "text" | "json_object" => Err(invalid("unexpected format attributes")),
        "json_schema" => {
            if fields
                .keys()
                .any(|key| !matches!(key.as_str(), "name" | "schema" | "description" | "strict"))
            {
                return Err(invalid("unsupported JSON schema format attribute"));
            }
            let valid_name = fields
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| {
                    !name.is_empty()
                        && name.len() <= 64
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                });
            if !valid_name {
                return Err(invalid(
                    "name must contain 1-64 ASCII letters, digits, underscores or hyphens",
                ));
            }
            if !fields.get("schema").is_some_and(Value::is_object) {
                return Err(invalid("schema must be an object"));
            }
            if fields.get("description").is_some_and(|v| !v.is_string()) {
                return Err(invalid("description must be a string"));
            }
            if fields
                .get("strict")
                .is_some_and(|v| !v.is_boolean() && !v.is_null())
            {
                return Err(invalid("strict must be a boolean or null"));
            }
            Ok(json!({"type":"json_schema", "json_schema":fields}))
        }
        _ => Err(invalid(
            "supported types are text, json_object and json_schema",
        )),
    }
}
