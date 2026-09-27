//! Bedrock ConverseStream using the Bedrock bearer-token authentication scheme.
//! `api_key` is an AWS Bedrock API key, sent as `Authorization: Bearer ...`.
//! IAM access keys, AWS profiles and SigV4 are NOT supported here: those
//! require request signing and must not be passed as bearer tokens.
//! `base_url` is the runtime endpoint root (region-specific); by default
//! us-east-1. The host must select the appropriate endpoint for other regions.

mod eventstream;

use async_trait::async_trait;
use futures_util::StreamExt;
use orca_harness_core::{
    Context, DeltaSink, Message, Model, ModelDelta, ModelError, ModelResponse, ToolCall,
    ToolSchema, Usage,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

pub struct BedrockModel {
    model: String,
    base_url: String,
    api_key: Option<String>,
    max_tokens: u64,
    reasoning_effort: Option<String>,
    reasoning_by_call: Mutex<HashMap<String, SavedReasoning>>,
}

#[derive(Clone)]
struct SavedReasoning {
    calls: Vec<ToolCall>,
    content: Option<String>,
    blocks: Vec<Value>,
}
impl SavedReasoning {
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

impl BedrockModel {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".into(),
            api_key: None,
            max_tokens: 8192,
            reasoning_effort: None,
            reasoning_by_call: Mutex::new(HashMap::new()),
        }
    }
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
    /// Bedrock bearer token, not an AWS access key or secret key.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }
    pub fn max_tokens(mut self, max: u64) -> Self {
        self.max_tokens = max;
        self
    }
    /// Opt-in model-specific output effort; sent in additionalModelRequestFields.
    /// Only use with models that document `output_config.effort` support.
    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    fn body(&self, context: &Context, tools: &[ToolSchema]) -> Result<Value, ModelError> {
        if self.max_tokens == 0 || self.max_tokens > i32::MAX as u64 {
            return Err(ModelError::Request(
                "Bedrock max_tokens out of range".into(),
            ));
        }
        // A different Context may belong to another conversation, not compaction.
        // Keep replay data for the lifetime of this model instance.
        let saved = self.reasoning_by_call.lock().unwrap();
        let mut system = Vec::new();
        let mut messages: Vec<Value> = Vec::new();
        for message in context.messages() {
            let (role, blocks): (&str, Vec<Value>) = match message {
                Message::System { content } => { if !content.is_empty() { system.push(json!({"text":content})); } continue; }
                Message::User { content, images } => {
                    let mut blocks = Vec::new();
                    if !content.is_empty() { blocks.push(json!({"text":content})); }
                    for image in images {
                        let format = image.media_type.strip_prefix("image/").unwrap_or("");
                        if !matches!(format, "png"|"jpeg"|"gif"|"webp") { return Err(ModelError::Request(format!("unsupported Bedrock image: {}", image.media_type))); }
                        // Bedrock JSON protocol expects base64 for blob fields.
                        blocks.push(json!({"image":{"format":format,"source":{"bytes":image.data}}}));
                    }
                    ("user", blocks)
                }
                Message::Assistant { content, tool_calls } => {
                    let mut blocks = tool_calls.iter().find_map(|call| saved.get(&call.id).filter(|entry| entry.matches(content, tool_calls))).map(|entry| entry.blocks.clone()).unwrap_or_default();
                    if let Some(text) = content.as_ref().filter(|s| !s.is_empty()) { blocks.push(json!({"text":text})); }
                    blocks.extend(tool_calls.iter().map(|call| json!({"toolUse":{"toolUseId":call.id,"name":call.name,"input":call.arguments}})));
                    ("assistant", blocks)
                }
                Message::Tool { results } => ("user", results.iter().map(|result| {
                    let (output, images) = crate::tool_images::split(&result.output);
                    let mut content = vec![json!({"text": crate::tool_images::text_of(&output)})];
                    for image in images {
                        let format = image.media_type.strip_prefix("image/").unwrap_or("");
                        if !matches!(format, "png"|"jpeg"|"gif"|"webp") { return Err(ModelError::Request(format!("unsupported Bedrock image: {}", image.media_type))); }
                        content.push(json!({"image":{"format":format,"source":{"bytes":image.data}}}));
                    }
                    Ok(json!({"toolResult":{"toolUseId":result.call_id,"content":content,"status":if result.is_error {"error"} else {"success"}}}))
                }).collect::<Result<Vec<_>, ModelError>>()?),
            };
            if blocks.is_empty() {
                continue;
            }
            // A signed reasoning block must lead its own assistant turn.
            let signed = blocks
                .first()
                .is_some_and(|b| b.get("reasoningContent").is_some());
            if let Some(last) = messages.last_mut().filter(|m| m["role"] == role && !signed) {
                last["content"].as_array_mut().unwrap().extend(blocks);
            } else {
                messages.push(json!({"role":role,"content":blocks}));
            }
        }
        let mut body = json!({"messages":messages,"inferenceConfig":{"maxTokens":self.max_tokens}});
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["toolConfig"] = json!({"tools":tools.iter().map(|t| json!({"toolSpec":{"name":t.name,"description":t.description,"inputSchema":{"json":t.parameters}}})).collect::<Vec<_>>()});
        }
        if let Some(effort) = &self.reasoning_effort {
            body["additionalModelRequestFields"] = json!({"output_config":{"effort":effort}});
        }
        Ok(body)
    }

    async fn run(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: Option<&dyn DeltaSink>,
    ) -> Result<ModelResponse, ModelError> {
        let key = self
            .api_key
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ModelError::Authentication(
                    "Bedrock bearer token required (AWS SigV4 unsupported)".into(),
                )
            })?;
        let body = self.body(context, tools)?;
        let mut header = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| ModelError::Authentication("invalid Bedrock bearer token".into()))?;
        header.set_sensitive(true);
        // ARN and inference-profile IDs may contain '/', which must remain one path segment.
        let model = self
            .model
            .as_bytes()
            .iter()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-._~".contains(b) {
                    (*b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect::<String>();
        let url = format!(
            "{}/model/{model}/converse-stream",
            self.base_url.trim_end_matches('/')
        );
        let response = crate::http::client()
            .post(url)
            .header(reqwest::header::AUTHORIZATION, header)
            .header("accept", "application/vnd.amazon.eventstream")
            .json(&body)
            .send()
            .await
            .map_err(|e| crate::http_error::transport_error(&e))?;
        let response = crate::http_error::check_response(response).await?;
        let mut bytes = response.bytes_stream();
        let mut decoder = eventstream::Decoder::default();
        let mut result = Accumulator::default();
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(|e| crate::http_error::transport_error(&e))?;
            for event in decoder.push(&chunk)? {
                if let Some(delta) = result.apply(event)? {
                    if let Some(sink) = sink {
                        sink.emit(delta).await;
                    }
                }
            }
        }
        decoder.finish()?;
        let (response, reasoning) = result.finish_collected()?;
        if let ModelResponse::ToolCalls { content, calls, .. } = &response {
            if !reasoning.is_empty() {
                let saved = SavedReasoning {
                    calls: calls.clone(),
                    content: content.clone(),
                    blocks: reasoning,
                };
                let mut cache = self.reasoning_by_call.lock().unwrap();
                for call in calls {
                    cache.insert(call.id.clone(), saved.clone());
                }
            }
        }
        Ok(response)
    }
}

#[async_trait]
impl Model for BedrockModel {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.run(context, tools, None).await
    }
    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        self.run(context, tools, Some(sink)).await
    }
}

fn invalid(s: impl Into<String>) -> ModelError {
    ModelError::InvalidResponse(format!("Bedrock: {}", s.into()))
}
#[derive(Default)]
struct Accumulator {
    blocks: BTreeMap<u64, Block>,
    text: String,
    started: bool,
    stop: Option<String>,
    usage: Option<Usage>,
}
struct Block {
    kind: &'static str,
    id: String,
    name: String,
    args: String,
    closed: bool,
    reasoning: String,
    signature: String,
    redacted: Option<String>,
}
impl Accumulator {
    fn apply(&mut self, event: Value) -> Result<Option<ModelDelta>, ModelError> {
        let kind = event["type"]
            .as_str()
            .ok_or_else(|| invalid("event type missing"))?;
        let data = &event[kind];
        if kind != "messageStart" && kind != "metadata" && !self.started {
            return Err(invalid("content before messageStart"));
        }
        if self.stop.is_some() && !matches!(kind, "metadata") {
            return Err(invalid("event after messageStop"));
        }
        match kind {
            "messageStart" => {
                if self.started {
                    return Err(invalid("duplicate messageStart"));
                }
                self.started = true;
            }
            "contentBlockStart" => {
                let index = data["contentBlockIndex"]
                    .as_u64()
                    .ok_or_else(|| invalid("block index missing"))?;
                let tool = &data["start"]["toolUse"];
                let block = Block {
                    kind: "tool",
                    id: tool["toolUseId"]
                        .as_str()
                        .ok_or_else(|| invalid("toolUseId missing"))?
                        .into(),
                    name: tool["name"]
                        .as_str()
                        .ok_or_else(|| invalid("tool name missing"))?
                        .into(),
                    args: String::new(),
                    closed: false,
                    reasoning: String::new(),
                    signature: String::new(),
                    redacted: None,
                };
                if self.blocks.insert(index, block).is_some() {
                    return Err(invalid("duplicate block"));
                }
            }
            "contentBlockDelta" => {
                let index = data["contentBlockIndex"]
                    .as_u64()
                    .ok_or_else(|| invalid("block index missing"))?;
                let delta = &data["delta"];
                if let Some(text) = delta["text"].as_str() {
                    let block = self.blocks.entry(index).or_insert_with(|| Block {
                        kind: "text",
                        id: String::new(),
                        name: String::new(),
                        args: String::new(),
                        closed: false,
                        reasoning: String::new(),
                        signature: String::new(),
                        redacted: None,
                    });
                    if block.closed || block.kind != "text" {
                        return Err(invalid("text delta on invalid block"));
                    }
                    self.text.push_str(text);
                    return Ok(Some(ModelDelta::Text { text: text.into() }));
                }
                if let Some(args) = delta["toolUse"]["input"].as_str() {
                    let block = self
                        .blocks
                        .get_mut(&index)
                        .ok_or_else(|| invalid("tool delta without start"))?;
                    if block.closed || block.kind != "tool" {
                        return Err(invalid("tool delta on invalid block"));
                    }
                    block.args.push_str(args);
                    return Ok(Some(ModelDelta::ToolInput { text: args.into() }));
                }
                if let Some(text) = delta["reasoningContent"]["text"]
                    .as_str()
                    .or_else(|| delta["reasoningContent"]["reasoningText"]["text"].as_str())
                {
                    let block = self.blocks.entry(index).or_insert_with(|| Block {
                        kind: "reasoning",
                        id: String::new(),
                        name: String::new(),
                        args: String::new(),
                        closed: false,
                        reasoning: String::new(),
                        signature: String::new(),
                        redacted: None,
                    });
                    if block.closed || block.kind != "reasoning" {
                        return Err(invalid("reasoning delta on invalid block"));
                    }
                    block.reasoning.push_str(text);
                    if let Some(sig) =
                        delta["reasoningContent"]["signature"].as_str().or_else(|| {
                            delta["reasoningContent"]["reasoningText"]["signature"].as_str()
                        })
                    {
                        block.signature.push_str(sig);
                    }
                    return Ok(Some(ModelDelta::Reasoning { text: text.into() }));
                }
                if delta["reasoningContent"].is_object() {
                    let block = self.blocks.entry(index).or_insert_with(|| Block {
                        kind: "reasoning",
                        id: String::new(),
                        name: String::new(),
                        args: String::new(),
                        closed: false,
                        reasoning: String::new(),
                        signature: String::new(),
                        redacted: None,
                    });
                    if block.closed || block.kind != "reasoning" {
                        return Err(invalid("reasoning delta on invalid block"));
                    }
                    if let Some(sig) =
                        delta["reasoningContent"]["signature"].as_str().or_else(|| {
                            delta["reasoningContent"]["reasoningText"]["signature"].as_str()
                        })
                    {
                        block.signature.push_str(sig);
                    }
                    if let Some(redacted) = delta["reasoningContent"]["redactedContent"].as_str() {
                        block.redacted = Some(redacted.into());
                    }
                } else {
                    return Err(invalid("unknown content delta"));
                }
            }
            "contentBlockStop" => {
                let index = data["contentBlockIndex"]
                    .as_u64()
                    .ok_or_else(|| invalid("block index missing"))?;
                let block = self
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(|| invalid("stop without block"))?;
                if block.closed {
                    return Err(invalid("duplicate block stop"));
                }
                block.closed = true;
            }
            "messageStop" => {
                if self.stop.is_some() {
                    return Err(invalid("duplicate messageStop"));
                }
                if self.blocks.values().any(|b| !b.closed) {
                    return Err(invalid("messageStop before block stop"));
                }
                self.stop = Some(
                    data["stopReason"]
                        .as_str()
                        .ok_or_else(|| invalid("stop reason missing"))?
                        .into(),
                );
            }
            "metadata" => {
                let u = &data["usage"];
                if u.is_object() {
                    for field in [
                        "inputTokens",
                        "outputTokens",
                        "cacheReadInputTokens",
                        "cacheWriteInputTokens",
                    ] {
                        if u.get(field).is_some_and(|v| v.as_u64().is_none()) {
                            return Err(invalid(format!("invalid usage {field}")));
                        }
                    }
                    let read = u["cacheReadInputTokens"].as_u64().unwrap_or(0);
                    let write = u["cacheWriteInputTokens"].as_u64().unwrap_or(0);
                    // AWS prompt-caching docs: inputTokens excludes both cache reads and writes.
                    // https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html
                    self.usage = Some(Usage {
                        input_tokens: u["inputTokens"].as_u64().unwrap_or(0),
                        output_tokens: u["outputTokens"].as_u64().unwrap_or(0),
                        cache_read_tokens: read,
                        cache_create_tokens: write,
                        ..Usage::default()
                    });
                }
            }
            _ => return Err(invalid(format!("unexpected event {kind}"))),
        }
        Ok(None)
    }
    #[cfg(test)]
    fn finish(self) -> Result<ModelResponse, ModelError> {
        self.finish_collected().map(|(response, _)| response)
    }
    fn finish_collected(self) -> Result<(ModelResponse, Vec<Value>), ModelError> {
        if !self.started || self.stop.is_none() || self.blocks.values().any(|b| !b.closed) {
            return Err(ModelError::IncompleteResponse {
                message: "Bedrock stream ended before messageStop or contentBlockStop".into(),
                usage: self.usage,
            });
        }
        match self.stop.as_deref() {
            Some("max_tokens" | "model_context_window_exceeded") => {
                return Err(ModelError::OutputLimit {
                    message: "Bedrock output limit".into(),
                    usage: self.usage,
                });
            }
            Some("guardrail_intervened" | "content_filtered") => {
                return Err(ModelError::ContentFiltered {
                    message: "Bedrock content filtered".into(),
                    usage: self.usage,
                });
            }
            Some("end_turn" | "stop_sequence" | "tool_use") => {}
            _ => return Err(invalid("unsupported stop reason")),
        }
        let mut calls = Vec::new();
        let mut reasoning = Vec::new();
        for block in self.blocks.into_values() {
            if block.kind == "reasoning" {
                if let Some(redacted) = block.redacted {
                    reasoning.push(json!({"reasoningContent":{"redactedContent":redacted}}));
                } else if !block.signature.is_empty() {
                    reasoning.push(json!({"reasoningContent":{"reasoningText":{"text":block.reasoning,"signature":block.signature}}}));
                }
                continue;
            }
            if block.kind != "tool" {
                continue;
            }
            let arguments = if block.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&block.args).map_err(|e| {
                    ModelError::MalformedToolArguments {
                        tool_name: block.name.clone(),
                        argument_bytes: block.args.len(),
                        finish_reason: self.stop.clone(),
                        message: e.to_string(),
                        usage: self.usage,
                    }
                })?
            };
            calls.push(ToolCall {
                id: block.id,
                name: block.name,
                arguments,
            });
        }
        if !calls.is_empty() {
            Ok((
                ModelResponse::ToolCalls {
                    content: (!self.text.is_empty()).then_some(self.text),
                    calls,
                    usage: self.usage,
                },
                reasoning,
            ))
        } else {
            Ok((
                ModelResponse::Final {
                    text: self.text,
                    usage: self.usage,
                },
                reasoning,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_core::{Image, ToolResult};

    #[test]
    fn request_preserves_tools_images_results_and_arn() {
        let model =
            BedrockModel::new("arn:aws:bedrock:us-east-1:123:inference-profile/foo").max_tokens(32);
        let mut ctx = Context::new();
        ctx.push_system("instructions");
        ctx.push_user_with_images(
            "look",
            vec![Image {
                media_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            }],
        );
        ctx.push_assistant_tool_calls(
            None,
            vec![ToolCall {
                id: "t1".into(),
                name: "read".into(),
                arguments: json!({"path":"/"}),
            }],
        );
        ctx.append_tool_results(vec![ToolResult {
            call_id: "t1".into(),
            tool_name: "read".into(),
            output: json!("ok"),
            is_error: false,
        }]);
        let body = model
            .body(
                &ctx,
                &[ToolSchema {
                    name: "read".into(),
                    description: "read".into(),
                    parameters: json!({"type":"object"}),
                }],
            )
            .unwrap();
        assert_eq!(
            body["messages"][0]["content"][1]["image"]["source"]["bytes"],
            "aGVsbG8="
        );
        assert_eq!(
            body["messages"][1]["content"][0]["toolUse"]["input"]["path"],
            "/"
        );
        assert_eq!(
            body["messages"][2]["content"][0]["toolResult"]["toolUseId"],
            "t1"
        );
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["type"],
            "object"
        );
        assert_eq!(body["system"][0]["text"], "instructions");
    }

    #[test]
    fn signed_reasoning_survives_sequential_a_b_a_contexts() {
        let model = BedrockModel::new("m");
        let calls = vec![ToolCall {
            id: "a".into(),
            name: "read".into(),
            arguments: json!({}),
        }];
        let signed =
            json!({"reasoningContent":{"reasoningText":{"text":"thinking","signature":"signed"}}});
        model.reasoning_by_call.lock().unwrap().insert(
            "a".into(),
            SavedReasoning {
                calls: calls.clone(),
                content: None,
                blocks: vec![signed.clone()],
            },
        );
        let mut ctx = Context::new();
        ctx.push_user("go");
        ctx.push_assistant_tool_calls(None, calls.clone());
        ctx.append_tool_results(vec![ToolResult {
            call_id: "a".into(),
            tool_name: "read".into(),
            output: json!("done"),
            is_error: false,
        }]);
        ctx.push_user("again");
        assert_eq!(
            model.body(&ctx, &[]).unwrap()["messages"][1]["content"][0],
            signed
        );
        assert_eq!(model.reasoning_by_call.lock().unwrap().len(), 1);
        let mut unrelated = Context::new();
        unrelated.push_user("conversation B");
        model.body(&unrelated, &[]).unwrap();
        assert_eq!(
            model.body(&ctx, &[]).unwrap()["messages"][1]["content"][0],
            signed
        );
        let mut altered = Context::new();
        altered.push_assistant_tool_calls(Some("changed".into()), calls);
        assert!(
            model.body(&altered, &[]).unwrap()["messages"][0]["content"][0]
                .get("reasoningContent")
                .is_none()
        );
        assert_eq!(model.reasoning_by_call.lock().unwrap().len(), 1);
        assert_eq!(
            model.body(&ctx, &[]).unwrap()["messages"][1]["content"][0],
            signed
        );
    }

    #[test]
    fn usage_input_tokens_are_already_cache_exclusive() {
        for (input, read, write) in [(12, 4, 3), (0, 100, 200), (12, 0, 0)] {
            let mut state = Accumulator::default();
            state
                .apply(json!({"type":"messageStart","messageStart":{}}))
                .unwrap();
            state
                .apply(json!({"type":"messageStop","messageStop":{"stopReason":"end_turn"}}))
                .unwrap();
            state
                .apply(json!({"type":"metadata","metadata":{"usage":{
                    "inputTokens":input,"outputTokens":3,
                    "cacheReadInputTokens":read,"cacheWriteInputTokens":write
                }}}))
                .unwrap();
            let response = state.finish().unwrap();
            let usage = response.usage().unwrap();
            assert_eq!(
                (
                    usage.input_tokens,
                    usage.cache_read_tokens,
                    usage.cache_create_tokens,
                    usage.output_tokens
                ),
                (input, read, write, 3)
            );
        }
    }

    #[test]
    fn accumulates_interleaved_blocks_usage_and_rejects_partial_tool_json() {
        let mut a = Accumulator::default();
        for event in [
            json!({"type":"messageStart","messageStart":{}}),
            json!({"type":"contentBlockStart","contentBlockStart":{"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"id","name":"read"}}}}),
            json!({"type":"contentBlockDelta","contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"hello"}}}),
            json!({"type":"contentBlockDelta","contentBlockDelta":{"contentBlockIndex":1,"delta":{"toolUse":{"input":"{\"a\":"}}}}),
            json!({"type":"contentBlockStop","contentBlockStop":{"contentBlockIndex":0}}),
            json!({"type":"contentBlockStop","contentBlockStop":{"contentBlockIndex":1}}),
            json!({"type":"messageStop","messageStop":{"stopReason":"tool_use"}}),
            json!({"type":"metadata","metadata":{"usage":{"inputTokens":12,"outputTokens":3,"cacheReadInputTokens":4}}}),
        ] {
            a.apply(event).unwrap();
        }
        assert!(matches!(
            a.finish(),
            Err(ModelError::MalformedToolArguments { .. })
        ));
        let mut a = Accumulator::default();
        a.apply(json!({"type":"messageStart","messageStart":{}}))
            .unwrap();
        a.apply(json!({"type":"messageStop","messageStop":{"stopReason":"end_turn"}}))
            .unwrap();
        a.apply(json!({"type":"metadata","metadata":{"usage":{"inputTokens":12,"outputTokens":3,"cacheReadInputTokens":4}}})).unwrap();
        assert_eq!(a.finish().unwrap().usage().unwrap().input_tokens, 12);
        let mut a = Accumulator::default();
        a.apply(json!({"type":"messageStart","messageStart":{}}))
            .unwrap();
        assert!(matches!(
            a.finish(),
            Err(ModelError::IncompleteResponse { .. })
        ));
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use orca_harness_core::ToolResult;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn crc(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in data {
            crc ^= *byte as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB88320 & (0u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }
    fn header(name: &str, value: &str) -> Vec<u8> {
        let mut h = vec![name.len() as u8];
        h.extend(name.as_bytes());
        h.push(7);
        h.extend((value.len() as u16).to_be_bytes());
        h.extend(value.as_bytes());
        h
    }
    fn frame(kind: &str, data: Value) -> Vec<u8> {
        let mut h = header(":message-type", "event");
        h.extend(header(":event-type", kind));
        let payload = json!({kind: data}).to_string();
        let mut b = Vec::new();
        b.extend(((16 + h.len() + payload.len()) as u32).to_be_bytes());
        b.extend((h.len() as u32).to_be_bytes());
        b.extend(crc(&b).to_be_bytes());
        b.extend(h);
        b.extend(payload.as_bytes());
        b.extend(crc(&b).to_be_bytes());
        b
    }
    #[tokio::test]
    async fn http_wire_stream_and_signed_tool_replay() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for turn in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    buf.extend(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                        let len: usize = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if buf.len() < end + 4 + len {
                            continue;
                        }
                        assert!(headers.contains("authorization: bearer secret"));
                        assert!(headers.contains("/model/arn%3aaws%2fabc/converse-stream"));
                        let body: Value =
                            serde_json::from_slice(&buf[end + 4..end + 4 + len]).unwrap();
                        if turn == 1 {
                            assert_eq!(
                                body["messages"][1]["content"][0]["reasoningContent"]
                                    ["reasoningText"],
                                json!({"text":"think","signature":"sig"})
                            );
                            assert_eq!(
                                body["messages"][2]["content"][0]["toolResult"]["toolUseId"],
                                "id"
                            );
                        }
                        break;
                    }
                }
                let mut stream = frame("messageStart", json!({"role":"assistant"}));
                if turn == 0 {
                    for (kind, data) in [
                        (
                            "contentBlockDelta",
                            json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"think"}}}),
                        ),
                        (
                            "contentBlockDelta",
                            json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"sig"}}}),
                        ),
                        ("contentBlockStop", json!({"contentBlockIndex":0})),
                        (
                            "contentBlockStart",
                            json!({"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"id","name":"read"}}}),
                        ),
                        (
                            "contentBlockDelta",
                            json!({"contentBlockIndex":1,"delta":{"toolUse":{"input":"{}"}}}),
                        ),
                        ("contentBlockStop", json!({"contentBlockIndex":1})),
                    ] {
                        stream.extend(frame(kind, data));
                    }
                }
                stream.extend(frame(
                    "messageStop",
                    json!({"stopReason":if turn == 0 {"tool_use"} else {"end_turn"}}),
                ));
                stream.extend(frame(
                    "metadata",
                    json!({"usage":{"inputTokens":12,"outputTokens":3,"cacheReadInputTokens":4}}),
                ));
                socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/vnd.amazon.eventstream\r\ncontent-length: {}\r\n\r\n", stream.len()).as_bytes()).await.unwrap();
                for part in stream.chunks(7) {
                    socket.write_all(part).await.unwrap();
                }
            }
        });
        let model = BedrockModel::new("arn:aws/abc")
            .base_url(url)
            .api_key("secret");
        let mut ctx = Context::new();
        ctx.push_user("read");
        let response = model.generate(&ctx, &[]).await.unwrap();
        let ModelResponse::ToolCalls { calls, usage, .. } = response else {
            panic!("expected tool");
        };
        assert_eq!(usage.unwrap().input_tokens, 12);
        ctx.push_assistant_tool_calls(None, calls);
        ctx.append_tool_results(vec![ToolResult {
            call_id: "id".into(),
            tool_name: "read".into(),
            output: json!("ok"),
            is_error: false,
        }]);
        assert!(matches!(
            model.generate(&ctx, &[]).await.unwrap(),
            ModelResponse::Final { .. }
        ));
        server.await.unwrap();
    }
}
