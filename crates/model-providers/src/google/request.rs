use super::GoogleModel;
use orca_harness_core::{Context, Message, ModelError, ToolSchema};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

impl GoogleModel {
    pub(super) fn body(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        signatures: &HashMap<String, Arc<[Value]>>,
    ) -> Result<Value, ModelError> {
        if self.max_tokens == Some(0) {
            return Err(ModelError::Request(
                "Google max_tokens must be positive".into(),
            ));
        }
        let mut system = Vec::new();
        let mut contents: Vec<Value> = Vec::new();
        let mut recent = crate::tool_images::Recent::new(context);
        for message in context.messages() {
            let (role, mut parts) = match message {
                Message::System { content } => {
                    if !content.is_empty() {
                        system.push(json!({"text":content}));
                    }
                    continue;
                }
                Message::User { content, images } => {
                    let mut parts = Vec::new();
                    if !content.is_empty() {
                        parts.push(json!({"text":content}));
                    }
                    parts.extend(images.iter().map(|image| json!({"inlineData":{"mimeType":image.media_type,"data":image.data}})));
                    ("user", parts)
                }
                Message::Assistant {
                    content,
                    tool_calls,
                } => {
                    let mut parts = Vec::new();
                    if let Some(text) = content.as_ref().filter(|s| !s.is_empty()) {
                        parts.push(json!({"text":text}));
                    }
                    for call in tool_calls {
                        let mut part =
                            json!({"functionCall":{"name":call.name,"args":call.arguments}});
                        // Only replay a provider id when the original call supplied one.
                        if let Some(saved) = signatures.get(&call.id) {
                            // A signature belongs to the original functionCall, not to an
                            // arbitrary later text part. Replay signed thought parts first.
                            if parts
                                .iter()
                                .all(|p| p.get("thought") != Some(&Value::Bool(true)))
                            {
                                let thoughts: Vec<Value> = saved
                                    .iter()
                                    .filter(|p| p["thought"] == true)
                                    .cloned()
                                    .collect();
                                parts.splice(0..0, thoughts);
                            }
                            if let Some(original) =
                                saved.iter().find(|p| p.get("functionCall").is_some())
                            {
                                if let Some(id) = original["functionCall"]["id"].as_str() {
                                    part["functionCall"]["id"] = json!(id);
                                }
                                if let Some(sig) = original.get("thoughtSignature") {
                                    part["thoughtSignature"] = sig.clone();
                                }
                            }
                        }
                        parts.push(part);
                    }
                    ("model", parts)
                }
                Message::Tool { results } => {
                    let mut parts = Vec::new();
                    for result in results {
                        let (output, images) = crate::tool_images::split(&result.output);
                        let fresh = recent.fresh(images);
                        let response = if result.is_error {
                            json!({"error":output})
                        } else {
                            json!({"output":output})
                        };
                        let mut part = json!({"functionResponse":{"name":result.tool_name,"response":response}});
                        if let Some(id) = signatures.get(&result.call_id).and_then(|saved| {
                            saved.iter().find_map(|p| p["functionCall"]["id"].as_str())
                        }) {
                            part["functionResponse"]["id"] = json!(id);
                        }
                        parts.push(part);
                        if !fresh.is_empty() {
                            let text = crate::tool_images::text_of(&output);
                            for part in crate::tool_images::interleave(&text, fresh) {
                                match part {
                                    crate::tool_images::Part::Text(text) => parts.push(json!({"text":text})),
                                    crate::tool_images::Part::Image(image) => parts.push(json!({"inlineData":{"mimeType":image.media_type,"data":image.data}})),
                                }
                            }
                        }
                    }
                    ("user", parts)
                }
            };
            if parts.is_empty() {
                continue;
            }
            if let Some(last) = contents.last_mut().filter(|last| last["role"] == role) {
                last["parts"].as_array_mut().unwrap().append(&mut parts);
            } else {
                contents.push(json!({"role":role,"parts":parts}));
            }
        }
        let mut body = json!({"contents":contents});
        if !system.is_empty() {
            body["systemInstruction"] = json!({"parts":system});
        }
        if !tools.is_empty() {
            body["tools"] = json!([{"functionDeclarations":tools.iter().map(|tool| json!({
                "name":tool.name,"description":tool.description,"parametersJsonSchema":tool.parameters
            })).collect::<Vec<_>>()}]);
        }
        if let Some(tokens) = self.max_tokens {
            body["generationConfig"]["maxOutputTokens"] = json!(tokens);
        }
        if let Some(effort) = &self.reasoning_effort {
            if !["minimal", "low", "medium", "high"].contains(&effort.as_str()) {
                return Err(ModelError::Request("invalid Gemini thinking level".into()));
            }
            let model = self.model.strip_prefix("models/").unwrap_or(&self.model);
            body["generationConfig"]["thinkingConfig"] = if model.starts_with("gemini-2.5-") {
                // Budgets valid for both 2.5 Pro (minimum 128) and Flash.
                let budget = match effort.as_str() {
                    "minimal" => 128,
                    "low" => 1024,
                    "medium" => 8192,
                    "high" => 24576,
                    _ => unreachable!(),
                };
                json!({"thinkingBudget":budget})
            } else if model.starts_with("gemini-3-") || model.starts_with("gemini-3.") {
                json!({"thinkingLevel":effort})
            } else {
                return Err(ModelError::Request(
                    "thinking effort requires Gemini 2.5 or 3".into(),
                ));
            };
        }
        Ok(body)
    }
}
