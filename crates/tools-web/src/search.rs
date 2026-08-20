//! `web_search` — query a search provider and return ranked hits.
//!
//! The provider is host configuration, not model input: the tool talks
//! to whatever [`SearchProvider`] it was constructed with. Ships with
//! [`Firecrawl`](crate::Firecrawl) (api.firecrawl.dev,
//! `FIRECRAWL_API_KEY`); implement the trait to plug in anything else.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use orca_harness_core::{Tool, ToolContext, ToolError, ToolSchema};

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    /// Short provider name for tool output (e.g. `firecrawl`).
    fn provider(&self) -> &str;
    async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchHit>, ToolError>;
}

pub struct WebSearchTool {
    provider: Arc<dyn SearchProvider>,
    default_count: usize,
    max_count: usize,
}

impl WebSearchTool {
    pub fn new(provider: Arc<dyn SearchProvider>) -> Self {
        Self {
            provider,
            default_count: 5,
            max_count: 20,
        }
    }

    /// Convenience: Firecrawl via `FIRECRAWL_API_KEY`.
    pub fn firecrawl_from_env() -> Result<Self, ToolError> {
        Ok(Self::new(Arc::new(crate::Firecrawl::from_env()?)))
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "web_search".into(),
            description: "Search the web and return ranked results with title, URL, and \
                snippet. Fetch a promising result with web_fetch."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "count": {"type": "integer", "default": 5, "description": "Number of results (max 20)."}
                },
                "required": ["query"]
            }),
        }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("`query` (string) is required"))?;
        if query.trim().is_empty() {
            return Err(ToolError::msg("`query` must not be empty"));
        }
        let count = input
            .get("count")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(self.default_count)
            .clamp(1, self.max_count);

        let hits = tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => return Err(ToolError::msg("cancelled")),
            r = self.provider.search(query, count) => r?,
        };
        let results: Vec<Value> = hits
            .iter()
            .map(|h| json!({ "title": h.title, "url": h.url, "snippet": h.snippet }))
            .collect();
        Ok(json!({
            "query": query,
            "provider": self.provider.provider(),
            "results": results,
        }))
    }
}
