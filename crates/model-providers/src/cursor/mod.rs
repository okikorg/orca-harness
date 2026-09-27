//! Cursor's native AgentService (not the OpenAI-compatible proxy).
//! See README.md for integration requirements and protocol limitations.
mod request;
#[cfg(test)]
mod tests;
mod wire;

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use orca_harness_core::{
    Context, DeltaSink, Model, ModelDelta, ModelError, ModelResponse, ToolCall, ToolSchema, Usage,
};
use prost::Message as _;
use std::{collections::HashMap, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex};
use wire::{Fields, Message as P};
type Result<T> = std::result::Result<T, ModelError>;
fn invalid(message: impl Into<String>) -> ModelError {
    ModelError::InvalidResponse(format!("Cursor: {}", message.into()))
}

pub const CURSOR_BASE_URL: &str = "https://agentn.us.api5.cursor.sh";
const CLIENT_VERSION: &str = "cli-2026.07.23-e383d2b";
const SERVICE: &str = "/agent.v1.AgentService/";

/// Clones share pending tool continuations. Keep the same model instance for
/// successive steps; a continuation is matched by its generated core call ID.
#[derive(Clone)]
pub struct CursorModel {
    model: String,
    base_url: String,
    key: String,
    pending: Arc<Mutex<HashMap<String, Session>>>,
}
impl CursorModel {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: CURSOR_BASE_URL.into(),
            key: String::new(),
            pending: Arc::default(),
        }
    }
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
    /// Cursor access token, not an OpenAI API key. No implicit environment lookup.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.key = key.into();
        self
    }

    async fn run(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse> {
        let mut resumed = None;
        if let Some(orca_harness_core::Message::Tool { results }) = context.messages().last() {
            let mut pending = self.pending.lock().await;
            for result in results {
                if let Some(mut session) = pending.remove(&result.call_id) {
                    if resumed.is_some() {
                        return Err(invalid("multiple live continuations in one context"));
                    }
                    session.reply(result)?;
                    resumed = Some(session);
                }
            }
            if resumed.is_none() && results.iter().any(|r| r.call_id.starts_with("cursor:")) {
                return Err(ModelError::Request(
                    "Cursor tool continuation expired or belongs to a different model instance"
                        .into(),
                ));
            }
        }
        let mut session = match resumed {
            Some(s) => s,
            None => self.start(context, tools).await?,
        };
        let mut text = String::new();
        let mut usage = None::<Usage>;
        loop {
            let (flag, payload) = session.next().await?;
            if flag == 2 {
                end_stream(&payload)?;
                return Ok(ModelResponse::Final { text, usage });
            }
            let outer = Fields::parse(&payload)?;
            if outer.has(1) {
                let update = Fields::parse(outer.bytes(1))?;
                for (tag, reasoning) in [(1, false), (4, true)] {
                    if update.has(tag) {
                        let delta = Fields::parse(update.bytes(tag))?.text(1)?;
                        if reasoning {
                            sink.emit(ModelDelta::Reasoning { text: delta }).await;
                        } else {
                            text.push_str(&delta);
                            sink.emit(ModelDelta::Text { text: delta }).await;
                        }
                    }
                }
                if update.has(8) {
                    let tokens = Fields::parse(update.bytes(8))?.number(1) as i32;
                    let u = usage.get_or_insert_with(Usage::default);
                    u.output_tokens = u.output_tokens.saturating_add(tokens.max(0) as u64);
                }
                if update.has(14) {
                    return Ok(ModelResponse::Final { text, usage });
                }
            }
            if outer.has(4) {
                session.kv(outer.bytes(4))?;
            }
            if outer.has(7) {
                return Err(invalid("interactive queries are not supported"));
            }
            if outer.has(2) {
                let exec = Fields::parse(outer.bytes(2))?;
                if !exec.has(11) {
                    session.send(
                        P::default()
                            .bytes(
                                5,
                                P::default()
                                    .bytes(
                                        2,
                                        P::default()
                                            .number(1, exec.number(1))
                                            .bytes(2, "Orca does not execute Cursor-native tools")
                                            .0,
                                    )
                                    .0,
                            )
                            .0,
                    )?;
                    continue;
                }
                let args = Fields::parse(exec.bytes(11))?;
                let raw_name = if args.bytes(5).is_empty() {
                    args.text(1)?
                } else {
                    args.text(5)?
                };
                let name = raw_name
                    .strip_prefix("mcp_orca_")
                    .unwrap_or(&raw_name)
                    .to_owned();
                if !session.tools.iter().any(|t| t == &name) {
                    return Err(invalid("server requested an unadvertised tool"));
                }
                let mut arguments = serde_json::Map::new();
                for entry in args.all(2) {
                    let entry = Fields::parse(entry)?;
                    let value = prost_types::Value::decode(entry.bytes(2))
                        .map_err(|e| invalid(e.to_string()))?;
                    arguments.insert(entry.text(1)?, request::from_proto(value));
                }
                // Use a local unique ID for routing; Cursor's exec id is preserved separately.
                let id = format!("cursor:{}", uuid::Uuid::new_v4());
                session.exec = Some((exec.number(1), exec.text(15)?));
                let call = ToolCall {
                    id: id.clone(),
                    name,
                    arguments: arguments.into(),
                };
                sink.emit(ModelDelta::ToolInput {
                    text: call.arguments.to_string(),
                })
                .await;
                let mut pending = self.pending.lock().await;
                pending.retain(|_, s| s.created.elapsed() < Duration::from_secs(3600));
                if pending.len() >= 32 {
                    return Err(ModelError::Request(
                        "Cursor pending continuation limit reached".into(),
                    ));
                }
                pending.insert(id, session);
                return Ok(ModelResponse::ToolCalls {
                    content: (!text.is_empty()).then_some(text),
                    calls: vec![call],
                    usage,
                });
            }
        }
    }
    async fn start(&self, context: &Context, tools: &[ToolSchema]) -> Result<Session> {
        let (payload, blobs) = request::build(&self.model, context, tools)?;
        let (tx, rx) = mpsc::unbounded_channel::<std::result::Result<Vec<u8>, std::io::Error>>();
        tx.send(Ok(wire::frame(&payload)?))
            .map_err(|_| invalid("request channel closed"))?;
        let body =
            futures_util::stream::unfold(rx, |mut rx| async { rx.recv().await.map(|x| (x, rx)) });
        let response = rpc(&self.base_url, &self.key, "Run", true)?
            .body(reqwest::Body::wrap_stream(body))
            .send()
            .await
            .map_err(|e| crate::http_error::transport_error(&e))?;
        let response = check_status(response).await?;
        let heartbeat_tx = tx.clone();
        let heartbeat = tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3600);
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                let packet =
                    wire::frame(&P::default().bytes(7, []).0).expect("empty heartbeat fits");
                if heartbeat_tx.send(Ok(packet)).is_err() {
                    break;
                }
            }
        });
        Ok(Session {
            tx,
            stream: Box::pin(response.bytes_stream().map(|r| {
                r.map(|b| b.to_vec())
                    .map_err(|e| crate::http_error::transport_error(&e))
            })),
            decoder: Default::default(),
            blobs,
            tools: tools.iter().map(|t| t.name.clone()).collect(),
            exec: None,
            heartbeat,
            created: std::time::Instant::now(),
        })
    }
}
#[async_trait]
impl Model for CursorModel {
    async fn generate(&self, context: &Context, tools: &[ToolSchema]) -> Result<ModelResponse> {
        self.run(context, tools, &|_: ModelDelta| {}).await
    }
    async fn generate_streaming(
        &self,
        context: &Context,
        tools: &[ToolSchema],
        sink: &dyn DeltaSink,
    ) -> Result<ModelResponse> {
        self.run(context, tools, sink).await
    }
}

struct Session {
    tx: mpsc::UnboundedSender<std::result::Result<Vec<u8>, std::io::Error>>,
    stream: Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>,
    decoder: wire::Decoder,
    blobs: request::Blobs,
    tools: Vec<String>,
    exec: Option<(u64, String)>,
    heartbeat: tokio::task::JoinHandle<()>,
    created: std::time::Instant,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.heartbeat.abort();
    }
}
impl Session {
    fn send(&self, payload: Vec<u8>) -> Result<()> {
        self.tx
            .send(Ok(wire::frame(&payload)?))
            .map_err(|_| ModelError::Request("Cursor request body closed".into()))
    }
    async fn next(&mut self) -> Result<(u8, Vec<u8>)> {
        loop {
            if self.created.elapsed() >= Duration::from_secs(3600) {
                return Err(ModelError::Request("Cursor session expired".into()));
            }
            if let Some(frame) = self.decoder.next()? {
                return Ok(frame);
            }
            let chunk = tokio::time::timeout(Duration::from_secs(120), self.stream.next())
                .await
                .map_err(|_| ModelError::Request("Cursor response idle timeout".into()))?;
            match chunk {
                Some(chunk) => self.decoder.push(&chunk?),
                None => {
                    return Err(ModelError::IncompleteResponse {
                        message: "Cursor stream closed without completion".into(),
                        usage: None,
                    });
                }
            }
        }
    }
    fn kv(&mut self, payload: &[u8]) -> Result<()> {
        let msg = Fields::parse(payload)?;
        let mut reply = P::default().number(1, msg.number(1));
        if msg.has(2) {
            let get = Fields::parse(msg.bytes(2))?;
            let data = self.blobs.get(get.bytes(1));
            let result = data.map(|b| P::default().bytes(1, b)).unwrap_or_default();
            reply = reply.bytes(2, result.0);
        } else if msg.has(3) {
            let set = Fields::parse(msg.bytes(3))?;
            let size: usize = self.blobs.values().map(Vec::len).sum();
            if size.saturating_add(set.bytes(2).len()) > 64 * 1024 * 1024 {
                return Err(invalid("KV storage limit exceeded"));
            }
            self.blobs
                .insert(set.bytes(1).to_vec(), set.bytes(2).to_vec());
            reply = reply.bytes(3, []);
        } else {
            return Err(invalid("unknown KV operation"));
        }
        self.send(P::default().bytes(3, reply.0).0)
    }
    fn reply(&mut self, result: &orca_harness_core::ToolResult) -> Result<()> {
        let (id, exec_id) = self
            .exec
            .take()
            .ok_or_else(|| invalid("no pending execution"))?;
        let (output, images) = crate::tool_images::split(&result.output);
        let text = crate::tool_images::text_of(&output);
        let content = P::default().bytes(1, P::default().bytes(1, text.as_bytes()).0);
        let mut success = P::default()
            .bytes(1, content.0)
            .number(2, u64::from(result.is_error));
        for img in images {
            let data = request::image(&orca_harness_core::Image {
                media_type: img.media_type.into(),
                data: img.data.into(),
            })?;
            let image = P::default().bytes(1, data).bytes(2, img.media_type);
            success = success.bytes(1, P::default().bytes(2, image.0).0);
        }
        let reply = P::default()
            .number(1, id)
            .bytes(15, exec_id)
            .bytes(11, P::default().bytes(1, success.0).0);
        self.send(P::default().bytes(2, reply.0).0)
    }
}
fn rpc(base: &str, key: &str, method: &str, streaming: bool) -> Result<reqwest::RequestBuilder> {
    if key.trim().is_empty() {
        return Err(ModelError::Authentication(
            "Cursor access token is required".into(),
        ));
    }
    // A separate client is necessary: the shared provider client does not enable h2 prior knowledge.
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = match CLIENT.get() {
        Some(client) => client.clone(),
        None => {
            let client = reqwest::Client::builder()
                .http2_prior_knowledge()
                .connect_timeout(Duration::from_secs(30))
                .build()
                .map_err(|e| crate::http_error::transport_error(&e))?;
            let _ = CLIENT.set(client.clone());
            client
        }
    };
    Ok(client
        .post(format!("{}{SERVICE}{method}", base.trim_end_matches('/')))
        .version(reqwest::Version::HTTP_2)
        .bearer_auth(key)
        .header(
            "content-type",
            if streaming {
                "application/connect+proto"
            } else {
                "application/proto"
            },
        )
        .header("connect-protocol-version", "1")
        .header("x-ghost-mode", "true")
        .header("x-cursor-client-type", "cli")
        .header("x-cursor-client-version", CLIENT_VERSION)
        .header("x-request-id", uuid::Uuid::new_v4().to_string()))
}
async fn check_status(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(ModelError::Authentication(format!("Cursor HTTP {status}")));
    }
    Err(ModelError::Request(format!("Cursor HTTP {status}")))
}
fn end_stream(payload: &[u8]) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|e| invalid(e.to_string()))?;
    if !value.is_object() {
        return Err(invalid("invalid Connect end envelope"));
    }
    if let Some(error) = value.get("error").filter(|e| !e.is_null()) {
        let code = error
            .get("code")
            .and_then(|c| c.as_str())
            .unwrap_or("unknown");
        // Do not echo arbitrary remote error text (may contain credentials or prompts).
        return Err(match code {
            "unauthenticated" | "permission_denied" => {
                ModelError::Authentication(format!("Cursor Connect {code}"))
            }
            _ => ModelError::Request(format!("Cursor Connect {code}")),
        });
    }
    Ok(())
}

/// Native unary GetUsableModels; no invented pricing or context-window defaults.
pub async fn list_models(base_url: &str, key: &str) -> Result<Vec<crate::catalog::ModelInfo>> {
    let response = rpc(base_url, key, "GetUsableModels", false)?
        .timeout(Duration::from_secs(60))
        .body(Vec::new())
        .send()
        .await
        .map_err(|e| crate::http_error::transport_error(&e))?;
    let mut stream = check_status(response).await?.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| crate::http_error::transport_error(&e))?;
        if bytes.len() + chunk.len() > wire::LIMIT {
            return Err(invalid("catalog too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let fields = Fields::parse(&bytes)?;
    let mut models = Vec::new();
    for row in fields.all(1) {
        let row = Fields::parse(row)?;
        let id = row.text(1)?;
        if id.trim().is_empty() {
            continue;
        }
        let mut name = None;
        for tag in [4, 5, 3] {
            let s = row.text(tag)?;
            if !s.trim().is_empty() {
                name = Some(s);
                break;
            }
        }
        models.push(crate::catalog::ModelInfo {
            id,
            name,
            context_length: None,
            pricing: None,
            reasoning: None,
        });
    }
    Ok(models)
}
