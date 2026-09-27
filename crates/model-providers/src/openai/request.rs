//! Chat-completions request encoding, separate from transport and response handling.
use super::OpenAiModel;
use orca_harness_core::{Context, Message, ToolCall, ToolSchema};
use serde_json::{json, Value};

#[derive(Clone)]
pub(super) struct SavedReasoning {
    calls: Vec<ToolCall>,
    content: Option<String>,
    text: String,
}

impl SavedReasoning {
    pub(super) fn new(calls: &[ToolCall], content: &Option<String>, text: String) -> Self {
        Self {
            calls: calls.to_vec(),
            content: content.clone(),
            text,
        }
    }

    fn matches(&self, content: &Option<String>, calls: &[ToolCall]) -> bool {
        self.content == *content
            && self.calls.len() == calls.len()
            && self
                .calls
                .iter()
                .zip(calls)
                .all(|(a, b)| a.id == b.id && a.name == b.name && a.arguments == b.arguments)
    }
}

impl OpenAiModel {
    pub(super) fn request_body(&self, context: &Context, tools: &[ToolSchema]) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": [],
        });
        let mut messages = encode_messages(context);
        if self.replay_reasoning_content {
            let mut cache = self.reasoning_by_call.lock().unwrap();
            // Encoding expands tool-result batches and image messages. Match
            // assistant turns separately rather than zipping unlike sequences.
            let assistants = context
                .messages()
                .iter()
                .filter(|message| matches!(message, Message::Assistant { .. }));
            for (wire, message) in messages
                .iter_mut()
                .filter(|wire| wire["role"] == "assistant")
                .zip(assistants)
            {
                if let Message::Assistant {
                    content,
                    tool_calls,
                } = message
                {
                    // Some thinking endpoints require the field on every assistant turn.
                    wire["reasoning_content"] = json!("");
                    if !tool_calls.is_empty() {
                        if let Some(saved) = cache.get(&tool_calls[0].id) {
                            if saved.matches(content, tool_calls) {
                                wire["reasoning_content"] = json!(saved.text);
                            }
                        }
                    }
                }
            }
            // Retire turns removed from the context, including all calls in a batch.
            cache.retain(|_, saved| {
                context.messages().iter().any(|message| {
                    matches!(message, Message::Assistant { content, tool_calls }
                    if saved.matches(content, tool_calls))
                })
            });
        }
        body["messages"] = Value::Array(messages);
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
            body[if self.max_completion_tokens {
                "max_completion_tokens"
            } else {
                "max_tokens"
            }] = json!(max_tokens);
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
    let mut recent = crate::tool_images::Recent::new(context);
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
                // `role: tool` content cannot hold images. After the whole
                // batch (tool messages must stay contiguous), one user message
                // carries them, each group labelled with its call id.
                let mut parts = Vec::new();
                for result in results {
                    let (output, images) = crate::tool_images::split(&result.output);
                    let mut message = json!({"role": "tool", "tool_call_id": result.call_id});
                    message["content"] = Value::String(output.to_string());
                    out.push(message);
                    // Each image is labelled with its call and its
                    // `[image N]` marker in that call's output.
                    for (number, image) in recent.fresh(images) {
                        parts.push(json!({
                            "type": "text",
                            "text": format!(
                                "[image {number}] from tool call {} ({}):",
                                result.call_id, result.tool_name
                            )
                        }));
                        let mut part = json!({"type": "image_url", "image_url": {}});
                        part["image_url"]["url"] = Value::String(image.data_url());
                        parts.push(part);
                    }
                }
                if !parts.is_empty() {
                    let mut message = json!({"role": "user", "content": null});
                    message["content"] = Value::Array(parts);
                    out.push(message);
                }
            }
        }
    }
    out
}
