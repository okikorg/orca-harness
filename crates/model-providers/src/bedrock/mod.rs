//! Bedrock ConverseStream using the Bedrock bearer-token authentication scheme.
//! `api_key` is an AWS Bedrock API key, sent as `Authorization: Bearer ...`.
//! IAM access keys, AWS profiles and SigV4 are NOT supported here: those
//! require request signing and must not be passed as bearer tokens.
//! `base_url` is the runtime endpoint root (region-specific); by default
//! us-east-1. The host must select the appropriate endpoint for other regions.

mod eventstream;

use crate::tool_images::Part;
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
    max_tokens: Option<u64>,
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
        self.content == *content && self.calls == calls
    }
}

impl BedrockModel {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".into(),
            api_key: None,
            max_tokens: None,
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
        self.max_tokens = Some(max);
        self
    }
    /// Request reasoning effort. Currently rejected because this adapter has no
    /// model metadata describing a supported Bedrock effort wire format.
    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    fn body(&self, context: &Context, tools: &[ToolSchema]) -> Result<Value, ModelError> {
        // Smithy integer range, not a guessed model output limit.
        if self
            .max_tokens
            .is_some_and(|max| max == 0 || max > i32::MAX as u64)
        {
            return Err(ModelError::Request(
                "Bedrock max_tokens out of range".into(),
            ));
        }
        if self.reasoning_effort.is_some() {
            return Err(ModelError::Request(
                "Bedrock reasoning effort unsupported without model capability metadata".into(),
            ));
        }
        // A different Context may belong to another conversation, not compaction.
        // Keep replay data for the lifetime of this model instance.
        let saved = self.reasoning_by_call.lock().unwrap();
        let mut recent = crate::tool_images::Recent::new(context);
        let mut system = Vec::new();
        let mut messages: Vec<Value> = Vec::new();
        for message in context.messages() {
            let (role, blocks): (&str, Vec<Value>) = match message {
                Message::System { content } => { if !content.is_empty() { system.push(json!({"text":content})); } continue; }
                Message::User { content, images } => {
                    let mut blocks = Vec::new();
                    if !content.is_empty() { blocks.push(json!({"text":content})); }
                    for image in images {
                        blocks.push(image_block(&image.media_type, &image.data)?);
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
                    let images = recent.fresh(images);
                    let text = crate::tool_images::text_of(&output);
                    // Images sit after their `[image N]` markers, in the order
                    // the tool returned text and images.
                    let content = if images.is_empty() {
                        vec![json!({"text": text})]
                    } else {
                        crate::tool_images::interleave(&text, images).into_iter().map(|part| match part {
                            Part::Text(text) => Ok(json!({"text": text})),
                            Part::Image(image) => image_block(image.media_type, image.data),
                        }).collect::<Result<_, ModelError>>()?
                    };
                    Ok(json!({"toolResult":{"toolUseId":result.call_id,"content":content,"status":if result.is_error {"error"} else {"success"}}}))
                }).collect::<Result<Vec<_>, ModelError>>()?),
            };
            // A signed reasoning block must lead its own assistant turn.
            crate::anthropic::push_turn(&mut messages, role, blocks, |b| {
                b.get("reasoningContent").is_some()
            });
        }
        // ConverseStream inferenceConfig and maxTokens are optional; omission
        // delegates the default to the model, including inference profiles.
        // https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_InferenceConfiguration.html
        let mut body = json!({"messages":messages});
        if let Some(max) = self.max_tokens {
            body["inferenceConfig"] = json!({"maxTokens":max});
        }
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["toolConfig"] = json!({"tools":tools.iter().map(|t| json!({"toolSpec":{"name":t.name,"description":t.description,"inputSchema":{"json":t.parameters}}})).collect::<Vec<_>>()});
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
        let response = crate::sse::send(
            crate::http::client()
                .post(url)
                .header(reqwest::header::AUTHORIZATION, header)
                .header("accept", "application/vnd.amazon.eventstream")
                .json(&body),
        )
        .await?;
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

/// A Bedrock image block; the JSON protocol expects base64 for blob fields.
fn image_block(media_type: &str, data: &str) -> Result<Value, ModelError> {
    let format = media_type.strip_prefix("image/").unwrap_or("");
    if !matches!(format, "png" | "jpeg" | "gif" | "webp") {
        return Err(ModelError::Request(format!(
            "unsupported Bedrock image: {media_type}"
        )));
    }
    Ok(json!({"image":{"format":format,"source":{"bytes":data}}}))
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
#[derive(Default)]
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
        let index = || {
            data["contentBlockIndex"]
                .as_u64()
                .ok_or_else(|| invalid("block index missing"))
        };
        match kind {
            "messageStart" => {
                if self.started {
                    return Err(invalid("duplicate messageStart"));
                }
                self.started = true;
            }
            "contentBlockStart" => {
                let index = index()?;
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
                    ..Default::default()
                };
                if self.blocks.insert(index, block).is_some() {
                    return Err(invalid("duplicate block"));
                }
            }
            "contentBlockDelta" => {
                let index = index()?;
                let delta = &data["delta"];
                if let Some(text) = delta["text"].as_str() {
                    let block = self.blocks.entry(index).or_insert_with(|| Block {
                        kind: "text",
                        ..Default::default()
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
                let reasoning = &delta["reasoningContent"];
                if !reasoning.is_object() {
                    return Err(invalid("unknown content delta"));
                }
                let block = self.blocks.entry(index).or_insert_with(|| Block {
                    kind: "reasoning",
                    ..Default::default()
                });
                if block.closed || block.kind != "reasoning" {
                    return Err(invalid("reasoning delta on invalid block"));
                }
                if let Some(sig) = reasoning["signature"]
                    .as_str()
                    .or_else(|| reasoning["reasoningText"]["signature"].as_str())
                {
                    block.signature.push_str(sig);
                }
                if let Some(text) = reasoning["text"]
                    .as_str()
                    .or_else(|| reasoning["reasoningText"]["text"].as_str())
                {
                    block.reasoning.push_str(text);
                    return Ok(Some(ModelDelta::Reasoning { text: text.into() }));
                }
                if let Some(redacted) = reasoning["redactedContent"].as_str() {
                    block.redacted = Some(redacted.into());
                }
            }
            "contentBlockStop" => {
                let block = self
                    .blocks
                    .get_mut(&index()?)
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
                    // AWS prompt-caching docs: inputTokens excludes both cache reads and writes.
                    // https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html
                    let mut usage = Usage::default();
                    for (field, target) in [
                        ("inputTokens", &mut usage.input_tokens),
                        ("outputTokens", &mut usage.output_tokens),
                        ("cacheReadInputTokens", &mut usage.cache_read_tokens),
                        ("cacheWriteInputTokens", &mut usage.cache_create_tokens),
                    ] {
                        if let Some(value) = u.get(field) {
                            *target = value
                                .as_u64()
                                .ok_or_else(|| invalid(format!("invalid usage {field}")))?;
                        }
                    }
                    self.usage = Some(usage);
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
            let arguments = crate::openai::parse_tool_arguments(
                &block.name,
                &block.args,
                self.stop.as_deref(),
                self.usage,
            )?;
            if block.id.is_empty() || block.name.is_empty() || !arguments.is_object() {
                return Err(invalid("invalid tool call"));
            }
            calls.push(ToolCall {
                id: block.id,
                name: block.name,
                arguments,
            });
        }
        if (self.stop.as_deref() == Some("tool_use")) != !calls.is_empty() {
            return Err(invalid("stop reason does not match tool calls"));
        }
        Ok((crate::response(self.text, calls, self.usage), reasoning))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_core::{Image, ToolResult};

    #[test]
    fn token_limit_is_unset_unless_explicit_and_effort_needs_metadata() {
        let ctx = Context::new();
        for id in [
            "unknown",
            "arn:aws:bedrock:us-east-1:123:inference-profile/foo",
        ] {
            assert!(BedrockModel::new(id)
                .body(&ctx, &[])
                .unwrap()
                .get("inferenceConfig")
                .is_none());
            assert_eq!(
                BedrockModel::new(id)
                    .max_tokens(12345)
                    .body(&ctx, &[])
                    .unwrap()["inferenceConfig"]["maxTokens"],
                12345
            );
            assert!(
                matches!(BedrockModel::new(id).reasoning_effort("high").body(&ctx, &[]), Err(ModelError::Request(message)) if message.contains("metadata"))
            );
        }
        for max in [0, i32::MAX as u64 + 1] {
            assert!(BedrockModel::new("m")
                .max_tokens(max)
                .body(&ctx, &[])
                .is_err());
        }
    }

    /// Apply `events` in order and finish the stream.
    fn collect(events: impl IntoIterator<Item = Value>) -> Result<ModelResponse, ModelError> {
        let mut state = Accumulator::default();
        for event in events {
            state.apply(event).unwrap();
        }
        state.finish()
    }

    fn start() -> Value {
        json!({"type":"messageStart","messageStart":{}})
    }

    fn stop(reason: &str) -> Value {
        json!({"type":"messageStop","messageStop":{"stopReason":reason}})
    }

    #[test]
    fn rejects_invalid_tool_arguments_and_stop_reason() {
        for (args, reason) in [("[]", "tool_use"), ("{}", "end_turn")] {
            assert!(matches!(
                collect([
                    start(),
                    json!({"type":"contentBlockStart","contentBlockStart":{"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"id","name":"read"}}}}),
                    json!({"type":"contentBlockDelta","contentBlockDelta":{"contentBlockIndex":0,"delta":{"toolUse":{"input":args}}}}),
                    json!({"type":"contentBlockStop","contentBlockStop":{"contentBlockIndex":0}}),
                    stop(reason),
                ]),
                Err(ModelError::InvalidResponse(_))
            ));
        }
        assert!(matches!(
            collect([start(), stop("tool_use")]),
            Err(ModelError::InvalidResponse(_))
        ));
    }

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
    fn tool_result_images_keep_the_newest_after_their_markers() {
        let call = ToolCall {
            id: "c".into(),
            name: "shot".into(),
            arguments: json!({}),
        };
        let mut ctx = Context::new();
        for step in 0..crate::tool_images::MAX_TOOL_IMAGES + 2 {
            ctx.append_tool_results(vec![ToolResult::ok(
                &call,
                json!({
                    "content": "shot [image 1] done",
                    "_images": [{"media_type": "image/png", "data": step.to_string()}]
                }),
            )]);
        }
        let body = BedrockModel::new("m").body(&ctx, &[]).unwrap();
        let results: Vec<&Value> = body["messages"][0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| &block["toolResult"]["content"])
            .collect();
        let images = results
            .iter()
            .flat_map(|content| content.as_array().unwrap())
            .filter(|block| block.get("image").is_some())
            .count();
        assert_eq!(images, crate::tool_images::MAX_TOOL_IMAGES);
        assert_eq!(*results[0], json!([{"text": "shot [image 1] done"}]));
        assert_eq!(
            *results[21],
            json!([
                {"text": "shot [image 1]"},
                {"image": {"format": "png", "source": {"bytes": "21"}}},
                {"text": " done"}
            ])
        );
    }

    #[test]
    fn usage_input_tokens_are_already_cache_exclusive() {
        for (input, read, write) in [(12, 4, 3), (0, 100, 200), (12, 0, 0)] {
            let response = collect([
                start(),
                stop("end_turn"),
                json!({"type":"metadata","metadata":{"usage":{
                    "inputTokens":input,"outputTokens":3,
                    "cacheReadInputTokens":read,"cacheWriteInputTokens":write
                }}}),
            ])
            .unwrap();
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
        assert!(matches!(
            collect([
                start(),
                json!({"type":"contentBlockStart","contentBlockStart":{"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"id","name":"read"}}}}),
                json!({"type":"contentBlockDelta","contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"hello"}}}),
                json!({"type":"contentBlockDelta","contentBlockDelta":{"contentBlockIndex":1,"delta":{"toolUse":{"input":"{\"a\":"}}}}),
                json!({"type":"contentBlockStop","contentBlockStop":{"contentBlockIndex":0}}),
                json!({"type":"contentBlockStop","contentBlockStop":{"contentBlockIndex":1}}),
                stop("tool_use"),
                json!({"type":"metadata","metadata":{"usage":{"inputTokens":12,"outputTokens":3,"cacheReadInputTokens":4}}}),
            ]),
            Err(ModelError::MalformedToolArguments { .. })
        ));
        assert!(matches!(
            collect([start()]),
            Err(ModelError::IncompleteResponse { .. })
        ));
    }
}

#[cfg(test)]
mod wire_tests {
    use super::eventstream::testing;
    use super::*;
    use crate::test_server::{serve, Reply};
    use orca_harness_core::ToolResult;

    fn frame(kind: &str, data: Value) -> Vec<u8> {
        testing::frame(kind, &json!({ kind: data }).to_string())
    }

    /// One ConverseStream response: messageStart, `events`, then usage.
    fn reply(events: Vec<(&str, Value)>) -> Reply {
        let mut body = frame("messageStart", json!({"role":"assistant"}));
        for (kind, data) in events {
            body.extend(frame(kind, data));
        }
        body.extend(frame(
            "metadata",
            json!({"usage":{"inputTokens":12,"outputTokens":3,"cacheReadInputTokens":4}}),
        ));
        Reply::new("200 OK", "application/vnd.amazon.eventstream", body)
    }

    #[tokio::test]
    async fn http_wire_stream_and_signed_tool_replay() {
        let (url, server) = serve(vec![
            reply(vec![
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
                ("messageStop", json!({"stopReason":"tool_use"})),
            ]),
            reply(vec![("messageStop", json!({"stopReason":"end_turn"}))]),
        ])
        .await;
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
        let requests = server.await.unwrap();
        for request in &requests {
            let head = request.lower();
            assert!(head.contains("authorization: bearer secret"));
            assert!(head.contains("/model/arn%3aaws%2fabc/converse-stream"));
        }
        let body = requests[1].json();
        assert_eq!(
            body["messages"][1]["content"][0]["reasoningContent"]["reasoningText"],
            json!({"text":"think","signature":"sig"})
        );
        assert_eq!(
            body["messages"][2]["content"][0]["toolResult"]["toolUseId"],
            "id"
        );
    }
}
