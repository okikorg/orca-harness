//! `web_fetch` — fetch a page and hand it to the model as markdown.
//!
//! HTML is converted to markdown (scripts/styles stripped); other text
//! types pass through as-is; binary types are refused. Redirects are
//! followed manually so the [`UrlPolicy`] re-vets every hop, and for
//! named hosts the connection is pinned to the vetted address.

use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::{Tool, ToolContext, ToolError, ToolSchema};
use reqwest::header::{CONTENT_TYPE, LOCATION};
use reqwest::{redirect, Url};

use crate::policy::UrlPolicy;

pub struct WebFetchTool {
    policy: UrlPolicy,
    /// Cap on downloaded body bytes.
    max_bytes: usize,
    /// Per-request (per-hop) timeout.
    timeout: Duration,
    max_redirects: usize,
    user_agent: String,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new(UrlPolicy::strict())
    }
}

impl WebFetchTool {
    pub fn new(policy: UrlPolicy) -> Self {
        Self {
            policy,
            max_bytes: 2 * 1024 * 1024,
            timeout: Duration::from_secs(30),
            max_redirects: 5,
            user_agent: format!("orca-harness-web/{}", env!("CARGO_PKG_VERSION")),
        }
    }

    pub fn max_bytes(mut self, bytes: usize) -> Self {
        self.max_bytes = bytes;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

fn is_ip_literal(host: &str) -> bool {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok()
}

/// text-ish content the model can read directly.
fn is_texty(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime.contains("json")
        || mime.contains("xml")
        || mime.contains("javascript")
        || mime.contains("x-www-form-urlencoded")
        || mime.is_empty()
}

/// Some servers omit or mislabel `Content-Type`. Detect document-shaped HTML
/// so markup never leaks into the model response just because the header is
/// wrong. Deliberately require a document-level marker to avoid treating plain
/// text containing an inline tag as a full HTML page.
fn looks_like_html(text: &str) -> bool {
    let start = text.trim_start().get(..512).unwrap_or(text.trim_start());
    let start = start.to_ascii_lowercase();
    start.starts_with("<!doctype html")
        || start.starts_with("<html")
        || start.contains("<head")
        || start.contains("<body")
}

fn html_to_markdown(html: &str) -> Result<String, ToolError> {
    html_to_markdown_rs::convert(html, None)
        .map(|result| result.content.unwrap_or_default())
        .map_err(|e| ToolError::msg(format!("HTML conversion failed: {e}")))
}

#[async_trait]
impl Tool for WebFetchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "web_fetch".into(),
            description: "Fetch a public http(s) URL. HTML comes back converted to \
                markdown; other text types come back raw; binary types are refused. \
                Responses over the size cap are truncated."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Absolute http(s) URL."}
                },
                "required": ["url"]
            }),
        }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let url_str = input
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`url` (string) is required"))?;
        let mut url =
            Url::parse(url_str).map_err(|e| ToolError::msg(format!("invalid URL: {e}")))?;

        // Follow redirects by hand so every hop is re-vetted and pinned.
        let mut hops = 0usize;
        let response = loop {
            let addrs = self.policy.check(&url).await?;
            let host = url
                .host_str()
                .ok_or_else(|| ToolError::msg("URL has no host"))?
                .to_string();
            let mut builder = reqwest::Client::builder()
                .redirect(redirect::Policy::none())
                .timeout(self.timeout)
                .user_agent(&self.user_agent);
            if !is_ip_literal(&host) {
                // Pin the connection to the address the policy vetted so a
                // second resolution cannot swap in a private target.
                builder = builder.resolve(&host, addrs[0]);
            }
            let client = builder
                .build()
                .map_err(|e| ToolError::msg(format!("client build failed: {e}")))?;

            let resp = tokio::select! {
                biased;
                _ = ctx.cancellation.cancelled() => return Err(ToolError::msg("cancelled")),
                r = client.get(url.clone()).send() => {
                    r.map_err(|e| ToolError::msg(format!("request failed: {e}")))?
                }
            };

            if resp.status().is_redirection() {
                hops += 1;
                if hops > self.max_redirects {
                    return Err(ToolError::msg("too many redirects"));
                }
                let location = resp
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| ToolError::msg("redirect without a Location header"))?;
                url = url
                    .join(location)
                    .map_err(|e| ToolError::msg(format!("bad redirect target: {e}")))?;
                continue;
            }
            break resp;
        };

        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let mime = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !is_texty(&mime) {
            return Err(ToolError::msg(format!(
                "unsupported content type `{mime}`; web_fetch only handles text"
            )));
        }

        let mut body: Vec<u8> = Vec::new();
        let mut truncated = false;
        let mut response = response;
        loop {
            let chunk = tokio::select! {
                biased;
                _ = ctx.cancellation.cancelled() => return Err(ToolError::msg("cancelled")),
                c = response.chunk() => {
                    c.map_err(|e| ToolError::msg(format!("read failed: {e}")))?
                }
            };
            let Some(chunk) = chunk else { break };
            body.extend_from_slice(&chunk);
            if body.len() >= self.max_bytes {
                truncated = true;
                body.truncate(self.max_bytes);
                break;
            }
        }

        let text = String::from_utf8_lossy(&body).into_owned();
        let is_html = mime.contains("html") || looks_like_html(&text);
        let content = if is_html {
            html_to_markdown(&text)?
        } else {
            text
        };

        Ok(json!({
            "url": url_str,
            "finalUrl": final_url,
            "status": status,
            "contentType": content_type,
            "content": content,
            "truncated": truncated,
        }))
    }
}
