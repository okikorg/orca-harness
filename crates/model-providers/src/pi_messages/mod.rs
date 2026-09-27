//! Radius Pi Messages gateway adapter with explicit host-supplied credentials.

use async_trait::async_trait;
use futures_util::StreamExt;
use orca_harness_core::{
    Context, DeltaSink, Message, Model, ModelDelta, ModelError, ModelResponse, ToolCall,
    ToolSchema, Usage,
};
use serde_json::{json, Value};

pub struct PiMessagesModel {
    model: String,
    base_url: String,
    api_key: Option<String>,
    max_tokens: u64,
    reasoning_effort: String,
}

impl PiMessagesModel {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: "https://radius.pi.dev/v1".into(),
            api_key: None,
            max_tokens: 8192,
            reasoning_effort: "off".into(),
        }
    }

    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }
    pub fn max_tokens(mut self, tokens: u64) -> Self {
        self.max_tokens = tokens;
        self
    }
    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = effort.into();
        self
    }

    fn payload(&self, context: &Context, tools: &[ToolSchema]) -> Value {
        let mut system = Vec::new();
        let mut messages = Vec::new();
        let mut recent = crate::tool_images::Recent::new(context);
        for message in context.messages() {
            match message {
                Message::System { content } => system.push(content.as_str()),
                Message::User { content, images } => {
                    let content = if images.is_empty() {
                        json!(content)
                    } else {
                        let mut blocks = vec![json!({"type":"text","text":content})];
                        blocks.extend(images.iter().map(|image| json!({"type":"image","data":image.data,"mimeType":image.media_type})));
                        json!(blocks)
                    };
                    messages.push(json!({"role":"user","content":content,"timestamp":0}));
                }
                Message::Assistant {
                    content,
                    tool_calls,
                } => {
                    let mut blocks = Vec::new();
                    if let Some(text) = content {
                        blocks.push(json!({"type":"text","text":text}));
                    }
                    blocks.extend(tool_calls.iter().map(|call| json!({"type":"toolCall","id":call.id,"name":call.name,"arguments":call.arguments})));
                    messages.push(json!({"role":"assistant","content":blocks,"api":"pi-messages","provider":"radius","model":self.model,"usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":if tool_calls.is_empty() { "stop" } else { "toolUse" },"timestamp":0}));
                }
                Message::Tool { results } => {
                    for result in results {
                        let (output, images) = crate::tool_images::split(&result.output);
                        let mut content = vec![
                            json!({"type":"text","text":crate::tool_images::text_of(&output)}),
                        ];
                        content.extend(recent.fresh(images).into_iter().map(|(_, image)| json!({"type":"image","data":image.data,"mimeType":image.media_type})));
                        messages.push(json!({"role":"toolResult","toolCallId":result.call_id,"toolName":result.tool_name,"content":content,"isError":result.is_error,"timestamp":0}));
                    }
                }
            }
        }
        json!({"model":self.model,"context":{"systemPrompt":if system.is_empty() { None } else { Some(system.join("\n\n")) },"messages":messages,"tools":tools},"options":{"temperature":null,"maxTokens":self.max_tokens,"reasoning":self.reasoning_effort,"sessionId":null}})
    }

    async fn generate_with(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: Option<&dyn DeltaSink>,
    ) -> Result<ModelResponse, ModelError> {
        let key = self
            .api_key
            .as_deref()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| ModelError::Authentication("Radius requires an API key".into()))?;
        let response = crate::http::client()
            .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
            .bearer_auth(key)
            .header("accept", "text/event-stream")
            .json(&self.payload(context, tools))
            .send()
            .await
            .map_err(|e| crate::http_error::transport_error(&e))?;
        let response = crate::http_error::check_response(response).await?;
        let mut bytes = response.bytes_stream();
        let mut frames = crate::sse::SseBuffer::default();
        let mut state = State::default();
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(|e| crate::http_error::transport_error(&e))?;
            for frame in frames.push(&chunk)? {
                let event: Value = serde_json::from_str(&frame)
                    .map_err(|e| ModelError::InvalidResponse(e.to_string()))?;
                if let Some(delta) = state.apply(&event)? {
                    if let Some(sink) = sink {
                        sink.emit(delta).await;
                    }
                }
                if state.terminal {
                    return state.finish();
                }
            }
        }
        Err(ModelError::IncompleteResponse {
            message: "Pi Messages stream ended without done".into(),
            usage: state.usage,
        })
    }
}

#[async_trait]
impl Model for PiMessagesModel {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.generate_with(context, tools, None).await
    }
    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        self.generate_with(context, tools, Some(sink)).await
    }
}

#[derive(Default)]
struct State {
    blocks: Vec<Block>,
    usage: Option<Usage>,
    terminal: bool,
    reason: String,
}

enum Block {
    Text(String),
    Thinking,
    Tool {
        id: String,
        name: String,
        args: String,
        final_args: Option<Value>,
    },
}

impl State {
    fn apply(&mut self, event: &Value) -> Result<Option<ModelDelta>, ModelError> {
        let kind = event["type"].as_str().unwrap_or_default();
        let index = event["contentIndex"].as_u64().unwrap_or(0) as usize;
        let delta = event["delta"].as_str().unwrap_or_default();
        let mut emitted = None;
        match kind {
            "text_start" => self.blocks.push(Block::Text(String::new())),
            "thinking_start" => self.blocks.push(Block::Thinking),
            "toolcall_start" => self.blocks.push(Block::Tool {
                id: event["id"].as_str().unwrap_or_default().into(),
                name: event["toolName"].as_str().unwrap_or_default().into(),
                args: String::new(),
                final_args: None,
            }),
            "text_delta" => {
                if let Some(Block::Text(text)) = self.blocks.get_mut(index) {
                    text.push_str(delta);
                    emitted = Some(ModelDelta::Text { text: delta.into() });
                }
            }
            "thinking_delta" => {
                if matches!(self.blocks.get(index), Some(Block::Thinking)) {
                    emitted = Some(ModelDelta::Reasoning { text: delta.into() });
                }
            }
            "toolcall_delta" => {
                if let Some(Block::Tool { args, .. }) = self.blocks.get_mut(index) {
                    args.push_str(delta);
                    emitted = Some(ModelDelta::ToolInput { text: delta.into() });
                }
            }
            "toolcall_end" => {
                if let Some(Block::Tool { final_args, .. }) = self.blocks.get_mut(index) {
                    *final_args = event["toolCall"].get("arguments").cloned();
                }
            }
            "done" | "error" => {
                self.usage = usage(&event["usage"]);
                self.terminal = true;
                self.reason = event["reason"].as_str().unwrap_or("stop").into();
                if kind == "error" {
                    return Err(ModelError::Request(
                        event["errorMessage"]
                            .as_str()
                            .unwrap_or("gateway request failed")
                            .into(),
                    ));
                }
            }
            _ => {}
        }
        Ok(emitted)
    }

    fn finish(self) -> Result<ModelResponse, ModelError> {
        if !self.terminal {
            return Err(ModelError::IncompleteResponse {
                message: "Pi Messages stream ended without done".into(),
                usage: self.usage,
            });
        }
        if self.reason == "length" {
            return Err(ModelError::OutputLimit {
                message: "Radius output limit reached".into(),
                usage: self.usage,
            });
        }
        let mut text = String::new();
        let mut calls = Vec::new();
        for block in self.blocks {
            match block {
                Block::Text(part) => text.push_str(&part),
                Block::Tool {
                    id,
                    name,
                    args,
                    final_args,
                } => {
                    let arguments = match final_args {
                        Some(value) => value,
                        None => serde_json::from_str(&args).map_err(|e| {
                            ModelError::MalformedToolArguments {
                                tool_name: name.clone(),
                                argument_bytes: args.len(),
                                finish_reason: Some(self.reason.clone()),
                                message: e.to_string(),
                                usage: self.usage,
                            }
                        })?,
                    };
                    if id.is_empty() || name.is_empty() || !arguments.is_object() {
                        return Err(ModelError::InvalidResponse(
                            "Pi Messages returned an invalid tool call".into(),
                        ));
                    }
                    calls.push(ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
                Block::Thinking => {}
            }
        }
        if calls.is_empty() {
            if self.reason == "toolUse" {
                return Err(ModelError::InvalidResponse(
                    "toolUse without tool calls".into(),
                ));
            }
            Ok(ModelResponse::Final {
                text,
                usage: self.usage,
            })
        } else {
            Ok(ModelResponse::ToolCalls {
                content: if text.is_empty() { None } else { Some(text) },
                calls,
                usage: self.usage,
            })
        }
    }
}

fn usage(value: &Value) -> Option<Usage> {
    value.as_object().map(|_| Usage {
        input_tokens: value["input"].as_u64().unwrap_or(0),
        output_tokens: value["output"].as_u64().unwrap_or(0),
        cache_read_tokens: value["cacheRead"].as_u64().unwrap_or(0),
        cache_create_tokens: value["cacheWrite"].as_u64().unwrap_or(0),
        reasoning_tokens: value["reasoning"].as_u64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_core::{Image, ToolResult};

    #[test]
    fn maps_images_tools_and_history() {
        let model = PiMessagesModel::new("balanced")
            .reasoning_effort("high")
            .max_tokens(42);
        let mut ctx = Context::new();
        ctx.push_system("instructions");
        ctx.push_user_with_images(
            "see",
            vec![Image {
                media_type: "image/png".into(),
                data: "YQ==".into(),
            }],
        );
        let call = ToolCall {
            id: "c".into(),
            name: "run".into(),
            arguments: json!({"a":1}),
        };
        ctx.push_assistant_tool_calls(Some("running".into()), vec![call.clone()]);
        ctx.append_tool_results(vec![ToolResult::ok(&call, json!("ok"))]);
        let payload = model.payload(
            &ctx,
            &[ToolSchema {
                name: "run".into(),
                description: "execute".into(),
                parameters: json!({"type":"object"}),
            }],
        );
        assert_eq!(payload["context"]["systemPrompt"], "instructions");
        assert_eq!(
            payload["context"]["messages"][0]["content"][1],
            json!({"type":"image","mimeType":"image/png","data":"YQ=="})
        );
        assert_eq!(
            payload["context"]["messages"][1]["content"][1]["arguments"],
            json!({"a":1})
        );
        assert_eq!(payload["context"]["messages"][2]["toolCallId"], "c");
        assert_eq!(payload["context"]["tools"][0]["name"], "run");
        assert_eq!(payload["options"]["reasoning"], "high");
    }

    #[test]
    fn collects_interleaved_deltas_and_terminal_usage() {
        let mut state = State::default();
        for event in [
            json!({"type":"text_start"}),
            json!({"type":"text_delta","contentIndex":0,"delta":"hello"}),
            json!({"type":"thinking_start","contentIndex":1}),
            json!({"type":"thinking_delta","contentIndex":1,"delta":"hmm"}),
            json!({"type":"toolcall_start","contentIndex":2,"id":"id","toolName":"run"}),
            json!({"type":"toolcall_delta","contentIndex":2,"delta":"{\"a\":"}),
            json!({"type":"toolcall_end","contentIndex":2,"toolCall":{"arguments":{"a":1}}}),
            json!({"type":"done","reason":"toolUse","usage":{"input":4,"output":5,"cacheRead":2,"cacheWrite":1,"reasoning":3}}),
        ] {
            state.apply(&event).unwrap();
        }
        match state.finish().unwrap() {
            ModelResponse::ToolCalls {
                content,
                calls,
                usage,
            } => {
                assert_eq!(content.as_deref(), Some("hello"));
                assert_eq!(calls[0].arguments, json!({"a":1}));
                assert_eq!(
                    usage.unwrap(),
                    Usage {
                        input_tokens: 4,
                        output_tokens: 5,
                        cache_read_tokens: 2,
                        cache_create_tokens: 1,
                        reasoning_tokens: Some(3)
                    }
                );
            }
            _ => panic!("expected tool calls"),
        }
    }

    #[test]
    fn reports_length_and_missing_terminal() {
        assert!(matches!(
            State::default().finish(),
            Err(ModelError::IncompleteResponse { .. })
        ));
        let mut state = State::default();
        state
            .apply(&json!({"type":"done","reason":"length","usage":{"output":9}}))
            .unwrap();
        assert!(matches!(
            state.finish(),
            Err(ModelError::OutputLimit { usage: Some(_), .. })
        ));
    }
}
