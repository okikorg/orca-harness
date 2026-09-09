use std::collections::HashMap;
use std::sync::Arc;

use orca_harness_core::{Context, Message, ModelError, ToolSchema};
use serde_json::{json, Value};

use super::AnthropicModel;

impl AnthropicModel {
    pub(super) fn request_body(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        thinking: &HashMap<String, Arc<[Value]>>,
    ) -> Result<Value, ModelError> {
        if self.max_tokens == 0 {
            return Err(ModelError::Request(
                "Anthropic max_tokens must be positive".into(),
            ));
        }
        let mut system = Vec::new();
        let mut messages: Vec<Value> = Vec::new();
        for message in context.messages() {
            let (role, blocks) = match message {
                Message::System { content } => {
                    if !content.is_empty() {
                        system.push(json!({"type": "text", "text": content}));
                    }
                    continue;
                }
                Message::User { content, images } => {
                    let mut blocks = Vec::new();
                    if !content.is_empty() {
                        blocks.push(json!({"type": "text", "text": content}));
                    }
                    blocks.extend(images.iter().map(|image| json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": image.media_type, "data": image.data}
                    })));
                    ("user", blocks)
                }
                Message::Assistant { content, tool_calls } => {
                    // Thinking blocks lead the assistant turn, ahead of text
                    // and tool_use. They are looked up by this turn's own call
                    // ids, so history with several tool turns stays aligned.
                    let mut blocks: Vec<Value> = tool_calls
                        .iter()
                        .find_map(|call| thinking.get(&call.id))
                        .map(|blocks| blocks.to_vec())
                        .unwrap_or_default();
                    if let Some(text) = content.as_ref().filter(|text| !text.is_empty()) {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                    blocks.extend(tool_calls.iter().map(|call| json!({
                        "type": "tool_use", "id": call.id, "name": call.name, "input": call.arguments
                    })));
                    ("assistant", blocks)
                }
                Message::Tool { results } => ("user", results.iter().map(|result| json!({
                    "type": "tool_result", "tool_use_id": result.call_id,
                    "content": result.output.as_str().map(str::to_owned).unwrap_or_else(|| result.output.to_string()),
                    "is_error": result.is_error
                })).collect()),
            };
            if blocks.is_empty() {
                continue;
            }
            // The native API alternates user/assistant turns. Consecutive tool
            // result batches and user messages are one user content array.
            // A turn led by thinking never merges: those blocks must stay
            // first in their own message, not land mid-array.
            let leads_with_thinking = matches!(
                blocks.first().and_then(|block| block["type"].as_str()),
                Some("thinking" | "redacted_thinking")
            );
            if let Some(last) = messages
                .last_mut()
                .filter(|last| last["role"] == role && !leads_with_thinking)
            {
                last["content"].as_array_mut().unwrap().extend(blocks);
            } else {
                messages.push(json!({"role": role, "content": blocks}));
            }
        }
        // Send no `thinking` key: it is the only shape every model accepts.
        // `disabled` is rejected by the Opus 5 and Fable families, `adaptive`
        // by Haiku 4.5, so let each model apply its own default instead of
        // encoding a model table here.
        let mut body = json!({
            "model": self.model, "max_tokens": self.max_tokens,
            "stream": true, "messages": messages
        });
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools
                .iter()
                .map(|tool| json!({
                    "name": tool.name, "description": tool.description,
                    "input_schema": input_schema(&tool.parameters)
                }))
                .collect::<Vec<_>>());
        }
        if self.prompt_cache {
            body["cache_control"] = json!({"type": "ephemeral"});
        }
        if let Some(temperature) = self.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(effort) = &self.reasoning_effort {
            body["output_config"] = json!({"effort": effort});
        }
        Ok(body)
    }
}

/// The Messages API rejects `oneOf`/`anyOf`/`allOf` at the top level of a tool
/// schema. Drop them there and leave nested occurrences intact; the tools that
/// use them already spell out the same action/field pairings in prose.
pub fn input_schema(parameters: &Value) -> Value {
    let mut schema = parameters.clone();
    if let Some(object) = schema.as_object_mut() {
        for keyword in ["oneOf", "anyOf", "allOf"] {
            object.remove(keyword);
        }
    }
    schema
}
