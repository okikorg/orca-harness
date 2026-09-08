//! OpenAI-compatible chat-completions adapter for Orca Harness.
//!
//! Works against any endpoint speaking the `/v1/chat/completions` protocol
//! (OpenAI, vLLM, llama.cpp server, most gateways). The kernel's Loop
//! contains no provider-specific logic; this crate is where the protocol
//! mapping lives.

mod stream;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

use orca_harness_core::{
    Context, DeltaSink, Message, Model, ModelError, ModelResponse, ToolCall, ToolSchema, Usage,
};

use self::stream::{ChunkAccumulator, SseLineBuffer};

fn parse_tool_arguments(
    tool_name: &str,
    arguments: &str,
    finish_reason: Option<&str>,
    usage: Option<Usage>,
) -> Result<Value, ModelError> {
    if arguments.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_str(arguments).map_err(|error| ModelError::MalformedToolArguments {
        tool_name: tool_name.to_string(),
        argument_bytes: arguments.len(),
        finish_reason: finish_reason.map(str::to_string),
        message: error.to_string(),
        usage,
    })
}

pub struct OpenAiModel {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    reasoning_effort: Option<String>,
    nested_reasoning: bool,
    parallel_tool_calls: Option<bool>,
    headers: Vec<(String, String)>,
    usage_accounting: bool,
    prompt_cache: bool,
    session_id: Option<String>,
    prompt_cache_key: Option<String>,
}

impl OpenAiModel {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: None,
            model: model.into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            nested_reasoning: false,
            parallel_tool_calls: None,
            headers: Vec::new(),
            usage_accounting: false,
            prompt_cache: false,
            session_id: None,
            prompt_cache_key: None,
        }
    }

    /// Ask the endpoint for detailed usage accounting (`usage: {include:
    /// true}`). OpenRouter needs this to report cached-token details;
    /// plain OpenAI rejects the parameter, so it is off by default.
    pub fn usage_accounting(mut self, enabled: bool) -> Self {
        self.usage_accounting = enabled;
        self
    }

    /// Add OpenRouter's normalized prompt-cache directive. Kept opt-in
    /// because not every OpenAI-compatible endpoint accepts this field.
    pub(crate) fn prompt_cache(mut self, enabled: bool) -> Self {
        self.prompt_cache = enabled;
        self
    }

    /// Keep gateway routing stable across every model step in one agent run.
    pub(crate) fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Keep OpenAI's automatic prompt cache routed by one run-stable key.
    pub fn prompt_cache_key(mut self, prompt_cache_key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(prompt_cache_key.into());
        self
    }

    /// Point at a different OpenAI-compatible endpoint, e.g. a local vLLM
    /// server. Pass the base up to (not including) `/chat/completions`.
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    pub fn temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Set OpenAI Chat Completions' flat reasoning-effort parameter.
    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self.nested_reasoning = false;
        self
    }

    /// Set OpenRouter's normalized nested reasoning parameter.
    pub fn nested_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self.nested_reasoning = true;
        self
    }

    /// Whether the endpoint may return multiple tool calls in one response.
    /// OpenAI defaults this to true, but some compatible servers default to
    /// single calls; unset, the field is omitted from the request entirely.
    pub fn parallel_tool_calls(mut self, enabled: bool) -> Self {
        self.parallel_tool_calls = Some(enabled);
        self
    }

    /// Attach a header to every request (gateway routing, attribution).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Identify the client to provider usage and diagnostics surfaces.
    pub fn user_agent(self, user_agent: impl Into<String>) -> Self {
        self.header(reqwest::header::USER_AGENT.as_str(), user_agent)
    }

    async fn post(&self, body: &Value) -> Result<reqwest::Response, ModelError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut request = self.client.post(&url).json(body);
        if let Some(api_key) = &self.api_key {
            request = request.bearer_auth(api_key);
        }
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request
            .send()
            .await
            .map_err(|e| crate::http_error::transport_error(&e))?;
        crate::http_error::check_response(response).await
    }

    fn request_body(&self, context: &Context, tools: &[ToolSchema]) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": encode_messages(context),
        });
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

fn encode_messages(context: &Context) -> Vec<Value> {
    let mut out = Vec::new();
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
                        json!({
                            "type": "image_url",
                            "image_url": {"url": crate::image_data_url(image)},
                        })
                    }));
                    out.push(json!({"role": "user", "content": parts}));
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
                                json!({
                                    "id": c.id,
                                    "type": "function",
                                    "function": {
                                        "name": c.name,
                                        // The wire format carries arguments
                                        // as a JSON-encoded string.
                                        "arguments": c.arguments.to_string(),
                                    }
                                })
                            })
                            .collect(),
                    );
                }
                out.push(m);
            }
            Message::Tool { results } => {
                for result in results {
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": result.call_id,
                        "content": result.output.to_string(),
                    }));
                }
            }
        }
    }
    out
}

#[derive(Deserialize)]
struct ChatCompletion {
    choices: Vec<Choice>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize, Default)]
pub(crate) struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: WirePromptTokensDetails,
    /// DeepSeek-style cache reporting, used when details are absent.
    #[serde(default)]
    prompt_cache_hit_tokens: u64,
}

#[derive(Deserialize, Default)]
struct WirePromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
    /// OpenRouter reports Anthropic cache writes here.
    #[serde(default)]
    cache_write_tokens: u64,
}

impl WireUsage {
    pub(crate) fn into_usage(self) -> Usage {
        // OpenAI-style `prompt_tokens` INCLUDES cached tokens; harness
        // Usage semantics keep them separate, so subtract to avoid
        // double-counting. Some providers (observed on OpenRouter) fold
        // cache writes into `cached_tokens` too — remove them from the
        // read figure. Matches pi-ai's normalization.
        let reported_cached = match self.prompt_tokens_details.cached_tokens {
            0 => self.prompt_cache_hit_tokens,
            cached => cached,
        };
        let cache_write = self.prompt_tokens_details.cache_write_tokens;
        let cache_read = if cache_write > 0 {
            reported_cached.saturating_sub(cache_write)
        } else {
            reported_cached
        };
        Usage {
            input_tokens: self
                .prompt_tokens
                .saturating_sub(cache_read)
                .saturating_sub(cache_write),
            output_tokens: self.completion_tokens,
            cache_read_tokens: cache_read,
            cache_create_tokens: cache_write,
        }
    }
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

#[derive(Deserialize)]
struct WireToolCall {
    id: String,
    function: WireFunction,
}

#[derive(Deserialize)]
struct WireFunction {
    name: String,
    arguments: String,
}

#[async_trait]
impl Model for OpenAiModel {
    async fn generate(
        &self,
        context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        let response = self.post(&self.request_body(context, tools)).await?;
        let body = response
            .text()
            .await
            .map_err(|e| crate::http_error::transport_error(&e))?;

        let completion: ChatCompletion = serde_json::from_str(&body)
            .map_err(|e| ModelError::InvalidResponse(format!("{e}: {body}")))?;
        let choice = completion
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ModelError::InvalidResponse("no choices in response".into()))?;

        let usage = completion.usage.map(WireUsage::into_usage);

        match choice.finish_reason.as_deref() {
            Some("length") => {
                return Err(ModelError::OutputLimit {
                    message:
                        "model output ended before the response completed; retry with a smaller payload"
                            .into(),
                    usage,
                });
            }
            Some("content_filter") => {
                return Err(ModelError::ContentFiltered {
                    message: "provider stopped generation".into(),
                    usage,
                });
            }
            _ => {}
        }

        if choice.message.tool_calls.is_empty() {
            return Ok(ModelResponse::Final {
                text: choice.message.content.unwrap_or_default(),
                usage,
            });
        }

        let calls = choice
            .message
            .tool_calls
            .into_iter()
            .map(|c| {
                parse_tool_arguments(
                    &c.function.name,
                    &c.function.arguments,
                    choice.finish_reason.as_deref(),
                    usage,
                )
                .map(|arguments| ToolCall {
                    id: c.id,
                    name: c.function.name,
                    arguments,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ModelResponse::ToolCalls {
            content: choice.message.content,
            calls,
            usage,
        })
    }

    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse, ModelError> {
        let mut body = self.request_body(context, tools);
        body["stream"] = json!(true);
        body["stream_options"] = json!({"include_usage": true});

        let response = self.post(&body).await?;
        let mut bytes = response.bytes_stream();
        let mut lines = SseLineBuffer::default();
        let mut accumulator = ChunkAccumulator::new();
        let mut done_observed = false;

        'body: while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(|e| crate::http_error::transport_error(&e))?;
            for payload in lines.push(&chunk) {
                if payload == "[DONE]" {
                    done_observed = true;
                    break 'body;
                }
                for delta in accumulator.apply(&payload)? {
                    sink.emit(delta).await;
                }
            }
        }

        if !done_observed {
            for payload in lines.finish() {
                if payload == "[DONE]" {
                    done_observed = true;
                    break;
                }
                for delta in accumulator.apply(&payload)? {
                    sink.emit(delta).await;
                }
            }
        }

        accumulator.finish(done_observed)
    }
}

#[cfg(test)]
mod tests;
