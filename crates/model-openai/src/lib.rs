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

use crate::stream::{ChunkAccumulator, SseLineBuffer};

pub struct OpenAiModel {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    parallel_tool_calls: Option<bool>,
    headers: Vec<(String, String)>,
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
            parallel_tool_calls: None,
            headers: Vec::new(),
        }
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
            .map_err(|e| ModelError::Request(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ModelError::Request(format!("HTTP {status}: {body}")));
        }
        Ok(response)
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
            Message::User { content } => {
                out.push(json!({"role": "user", "content": content}));
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
}

#[derive(Deserialize, Default)]
struct WirePromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

impl WireUsage {
    pub(crate) fn into_usage(self) -> Usage {
        Usage {
            input_tokens: self.prompt_tokens,
            output_tokens: self.completion_tokens,
            cache_read_tokens: self.prompt_tokens_details.cached_tokens,
            cache_create_tokens: 0,
        }
    }
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
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
            .map_err(|e| ModelError::Request(e.to_string()))?;

        let completion: ChatCompletion = serde_json::from_str(&body)
            .map_err(|e| ModelError::InvalidResponse(format!("{e}: {body}")))?;
        let choice = completion
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ModelError::InvalidResponse("no choices in response".into()))?;

        let usage = completion.usage.map(WireUsage::into_usage);

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
                let arguments = if c.function.arguments.trim().is_empty() {
                    Ok(Value::Object(Default::default()))
                } else {
                    serde_json::from_str(&c.function.arguments)
                };
                arguments
                    .map(|arguments| ToolCall {
                        id: c.id,
                        name: c.function.name,
                        arguments,
                    })
                    .map_err(|e| ModelError::InvalidResponse(format!("bad tool arguments: {e}")))
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

        'body: while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(|e| ModelError::Request(e.to_string()))?;
            for payload in lines.push(&chunk) {
                if payload == "[DONE]" {
                    break 'body;
                }
                for delta in accumulator.apply(&payload)? {
                    sink.emit(delta).await;
                }
            }
        }

        accumulator.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schemas() -> Vec<ToolSchema> {
        vec![ToolSchema {
            name: "grep".into(),
            description: "search".into(),
            parameters: json!({"type": "object"}),
        }]
    }

    fn context() -> Context {
        let mut context = Context::new();
        context.push_user("hi");
        context
    }

    #[test]
    fn parallel_tool_calls_is_omitted_when_unset() {
        let model = OpenAiModel::new("m");
        let body = model.request_body(&context(), &schemas());
        assert!(body.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn parallel_tool_calls_is_sent_when_set() {
        let model = OpenAiModel::new("m").parallel_tool_calls(true);
        let body = model.request_body(&context(), &schemas());
        assert_eq!(body["parallel_tool_calls"], json!(true));

        let model = OpenAiModel::new("m").parallel_tool_calls(false);
        let body = model.request_body(&context(), &schemas());
        assert_eq!(body["parallel_tool_calls"], json!(false));
    }

    #[test]
    fn parallel_tool_calls_is_omitted_without_tools() {
        // OpenAI rejects the field on tool-less requests.
        let model = OpenAiModel::new("m").parallel_tool_calls(true);
        let body = model.request_body(&context(), &[]);
        assert!(body.get("parallel_tool_calls").is_none());
    }
}
