//! `web_crawl` — crawl a site and return its pages as markdown.
//!
//! Backed by [`Firecrawl`]: the tool starts a crawl job and polls it
//! until the job finishes, the wait budget runs out, or the call is
//! cancelled. Timing out is not an error — whatever pages were scraped
//! by then come back with `"timedOut": true` so the model can decide
//! whether partial coverage is enough.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::{Tool, ToolContext, ToolError, ToolSchema};

use crate::firecrawl::Firecrawl;

pub struct WebCrawlTool {
    fc: Arc<Firecrawl>,
    default_pages: usize,
    max_pages: usize,
    max_chars_per_page: usize,
    poll_interval: Duration,
    max_wait: Duration,
}

impl WebCrawlTool {
    pub fn new(fc: Arc<Firecrawl>) -> Self {
        Self {
            fc,
            default_pages: 10,
            max_pages: 25,
            max_chars_per_page: 20_000,
            poll_interval: Duration::from_secs(2),
            max_wait: Duration::from_secs(120),
        }
    }

    /// Convenience: Firecrawl via `FIRECRAWL_API_KEY`.
    pub fn from_env() -> Result<Self, ToolError> {
        Ok(Self::new(Arc::new(Firecrawl::from_env()?)))
    }

    /// Cap on the `limit` parameter (and its default ceiling).
    pub fn max_pages(mut self, n: usize) -> Self {
        self.max_pages = n.max(1);
        self.default_pages = self.default_pages.min(self.max_pages);
        self
    }

    /// Per-page markdown cap in characters.
    pub fn page_chars(mut self, n: usize) -> Self {
        self.max_chars_per_page = n.max(1);
        self
    }

    /// How often to poll the crawl job.
    pub fn poll_interval(mut self, d: Duration) -> Self {
        self.poll_interval = d;
        self
    }

    /// Total wait budget before returning whatever has been scraped.
    pub fn max_wait(mut self, d: Duration) -> Self {
        self.max_wait = d;
        self
    }
}

#[async_trait]
impl Tool for WebCrawlTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "web_crawl".into(),
            description: format!(
                "Crawl a website starting from a URL and return its pages as markdown. \
                 Use web_fetch for a single known page; use this to read a whole \
                 section of a site (docs, blog). Up to {} pages per call.",
                self.max_pages
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Starting URL, e.g. https://docs.example.com."},
                    "limit": {
                        "type": "integer",
                        "default": self.default_pages,
                        "description": format!("Maximum pages to crawl (max {}).", self.max_pages)
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let url = input
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`url` (string) is required"))?;
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(ToolError::msg("`url` must start with http:// or https://"));
        }
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(self.default_pages)
            .clamp(1, self.max_pages);

        let job_id = tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => return Err(ToolError::msg("cancelled")),
            r = self.fc.start_crawl(url, limit) => r?,
        };

        let started = Instant::now();
        let (last, timed_out) = loop {
            let status = tokio::select! {
                biased;
                _ = ctx.cancellation.cancelled() => return Err(ToolError::msg("cancelled")),
                r = self.fc.crawl_status(&job_id) => r?,
            };
            match status["status"].as_str() {
                Some("scraping") | None => {}
                _ => break (status, false),
            }
            if started.elapsed() >= self.max_wait {
                break (status, true);
            }
            tokio::select! {
                biased;
                _ = ctx.cancellation.cancelled() => return Err(ToolError::msg("cancelled")),
                _ = tokio::time::sleep(self.poll_interval) => {}
            }
        };

        let pages: Vec<Value> = last["data"]
            .as_array()
            .map(|data| {
                data.iter()
                    .map(|page| {
                        let markdown = page["markdown"].as_str().unwrap_or("");
                        let (kept, truncated) = clip_chars(markdown, self.max_chars_per_page);
                        json!({
                            "url": page["metadata"]["sourceURL"].as_str().unwrap_or(""),
                            "title": page["metadata"]["title"].as_str().unwrap_or(""),
                            "markdown": kept,
                            "truncated": truncated,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(json!({
            "url": url,
            "jobId": job_id,
            "status": last["status"].as_str().unwrap_or("unknown"),
            "totalPages": last["total"].as_u64().unwrap_or(pages.len() as u64),
            "pagesReturned": pages.len(),
            "timedOut": timed_out,
            "pages": pages,
        }))
    }
}

/// First `max` characters of `s`, with a flag when anything was dropped.
fn clip_chars(s: &str, max: usize) -> (&str, bool) {
    match s.char_indices().nth(max) {
        Some((byte, _)) => (&s[..byte], true),
        None => (s, false),
    }
}

#[cfg(test)]
mod tests {
    use super::clip_chars;

    #[test]
    fn clipping_respects_char_boundaries() {
        assert_eq!(clip_chars("abcdef", 3), ("abc", true));
        assert_eq!(clip_chars("abc", 3), ("abc", false));
        assert_eq!(clip_chars("héllo", 2), ("hé", true));
        assert_eq!(clip_chars("", 5), ("", false));
    }
}
