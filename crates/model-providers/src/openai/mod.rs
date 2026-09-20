//! OpenAI-compatible chat-completions adapter for Orca Harness.
//!
//! Works against any endpoint speaking the `/v1/chat/completions` protocol
//! (OpenAI, vLLM, llama.cpp server, most gateways). The kernel's Loop
//! contains no provider-specific logic; this crate is where the protocol
//! mapping lives.

mod request;
mod stream;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

use orca_harness_core::{
    Context, DeltaSink, Model, ModelError, ModelResponse, ToolCall, ToolSchema, Usage,
};

use self::stream::ChunkAccumulator;
use crate::sse::SseLineBuffer;
#[cfg(test)]
use request::encode_messages;

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

    fn prepare_request(&self, body: &Value) -> reqwest::RequestBuilder {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut request = crate::http::client().post(&url).json(body);
        if let Some(api_key) = &self.api_key {
            request = request.bearer_auth(api_key);
        }
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request
    }

    async fn post(&self, body: Value) -> Result<reqwest::Response, ModelError> {
        let request = self.prepare_request(&body);
        // RequestBuilder owns the serialized bytes; release the JSON tree
        // before waiting for a potentially long provider response.
        drop(body);
        let response = request
            .send()
            .await
            .map_err(|e| crate::http_error::transport_error(&e))?;
        crate::http_error::check_response(response).await
    }
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
    #[serde(default)]
    completion_tokens_details: WireCompletionTokensDetails,
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

#[derive(Deserialize, Default)]
struct WireCompletionTokensDetails {
    reasoning_tokens: Option<u64>,
}

impl WireUsage {
    pub(crate) fn into_usage(self) -> Usage {
        // OpenAI-style `prompt_tokens` INCLUDES cached tokens; harness
        // Usage semantics keep them separate, so subtract to avoid
        // double-counting. `cached_tokens` is reads-only and disjoint
        // from `cache_write_tokens`, confirmed against live OpenRouter
        // traffic: a request that both reads and writes cache reports
        // both fields independently, summing to prompt_tokens. Do not
        // subtract cache_write from cached_tokens here again; that
        // double-counted every cache write into input_tokens (#19).
        let cache_read = match self.prompt_tokens_details.cached_tokens {
            0 => self.prompt_cache_hit_tokens,
            cached => cached,
        };
        let cache_write = self.prompt_tokens_details.cache_write_tokens;
        Usage {
            input_tokens: self
                .prompt_tokens
                .saturating_sub(cache_read)
                .saturating_sub(cache_write),
            output_tokens: self.completion_tokens,
            cache_read_tokens: cache_read,
            cache_create_tokens: cache_write,
            reasoning_tokens: self.completion_tokens_details.reasoning_tokens,
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
        let response = self.post(self.request_body(context, tools)).await?;
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

        let response = self.post(body).await?;
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

#[cfg(test)]
mod request_bench;
