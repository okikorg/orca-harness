//! Firecrawl API client: the search and crawl provider.
//!
//! One client backs both `web_search` (POST /v2/search) and `web_crawl`
//! (POST /v2/crawl + status polling). The base URL is overridable for
//! tests, proxies, and self-hosted Firecrawl instances. Page fetching
//! happens on Firecrawl's servers, so the [`UrlPolicy`](crate::UrlPolicy)
//! SSRF guard does not apply here — only `web_fetch` touches the network
//! from the host directly.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::ToolError;

use crate::search::{SearchHit, SearchProvider};

pub struct Firecrawl {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl Firecrawl {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.firecrawl.dev".into(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
        }
    }

    /// Read the key from `FIRECRAWL_API_KEY`.
    pub fn from_env() -> Result<Self, ToolError> {
        std::env::var("FIRECRAWL_API_KEY")
            .map(Self::new)
            .map_err(|_| ToolError::msg("FIRECRAWL_API_KEY is not set"))
    }

    /// Override the API base URL (tests, proxies, self-hosted).
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into().trim_end_matches('/').to_string();
        self
    }

    async fn request(&self, req: reqwest::RequestBuilder) -> Result<Value, ToolError> {
        let resp = req
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .map_err(|e| ToolError::msg(format!("firecrawl request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ToolError::msg(format!("firecrawl API returned {status}")));
        }
        resp.json()
            .await
            .map_err(|e| ToolError::msg(format!("firecrawl response was not JSON: {e}")))
    }

    /// Start a crawl job; returns the job id to poll with [`crawl_status`].
    ///
    /// [`crawl_status`]: Self::crawl_status
    pub async fn start_crawl(&self, url: &str, limit: usize) -> Result<String, ToolError> {
        let body = json!({
            "url": url,
            "limit": limit,
            "scrapeOptions": { "formats": ["markdown"], "onlyMainContent": true },
        });
        let resp = self
            .request(
                self.client
                    .post(format!("{}/v2/crawl", self.base_url))
                    .json(&body),
            )
            .await?;
        resp["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| ToolError::msg(format!("crawl start returned no job id: {resp}")))
    }

    /// Poll a crawl job. `status` in the result is `scraping`, `completed`,
    /// or `failed`; `data` holds the pages scraped so far.
    pub async fn crawl_status(&self, job_id: &str) -> Result<Value, ToolError> {
        self.request(
            self.client
                .get(format!("{}/v2/crawl/{job_id}", self.base_url)),
        )
        .await
    }
}

#[async_trait]
impl SearchProvider for Firecrawl {
    fn provider(&self) -> &str {
        "firecrawl"
    }

    async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchHit>, ToolError> {
        let body = self
            .request(
                self.client
                    .post(format!("{}/v2/search", self.base_url))
                    .json(&json!({ "query": query, "limit": count })),
            )
            .await?;
        Ok(parse_search_hits(&body))
    }
}

fn parse_search_hits(body: &Value) -> Vec<SearchHit> {
    body["data"]["web"]
        .as_array()
        .map(|results| {
            results
                .iter()
                .map(|r| SearchHit {
                    title: r["title"].as_str().unwrap_or("").to_string(),
                    url: r["url"].as_str().unwrap_or("").to_string(),
                    snippet: r["description"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_v2_search_shape() {
        let body = json!({
            "success": true,
            "data": { "web": [
                {"url": "https://a.example", "title": "A", "description": "first", "position": 1},
                {"url": "https://b.example", "title": "B", "description": "second", "position": 2},
            ]}
        });
        let hits = parse_search_hits(&body);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "A");
        assert_eq!(hits[1].snippet, "second");
    }

    #[test]
    fn missing_or_malformed_data_yields_no_hits() {
        assert!(parse_search_hits(&json!({"success": true})).is_empty());
        assert!(parse_search_hits(&json!({"data": {"web": "nope"}})).is_empty());
    }
}
