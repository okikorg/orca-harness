//! # Orca Harness Web Tools
//!
//! `web_fetch`, `web_search`, and `web_crawl`, deliberately outside the
//! core tool set: they open network egress, carry an HTML-parsing
//! dependency, and (for search/crawl) need provider credentials — all
//! host opt-ins, not kernel assumptions.
//!
//! `web_fetch` touches the network from the host, so every fetch goes
//! through a [`UrlPolicy`] SSRF guard that vets the scheme and
//! destination address of each redirect hop. Search and crawling are
//! delegated to [`Firecrawl`] (`FIRECRAWL_API_KEY`), which fetches on
//! its own servers; the [`SearchProvider`] trait lets a host swap in a
//! different search backend.
//!
//! ```no_run
//! use std::sync::Arc;
//! use orca_harness_core::Agent;
//! use orca_harness_tools_web::{UrlPolicy, WebCrawlTool, WebFetchTool, WebSearchTool};
//!
//! # fn example(model: impl orca_harness_core::Model) -> Result<(), Box<dyn std::error::Error>> {
//! let mut agent = Agent::new(model)
//!     .tool_arc(Arc::new(WebFetchTool::new(UrlPolicy::strict())));
//! if let Ok(search) = WebSearchTool::firecrawl_from_env() {
//!     agent = agent.tool_arc(Arc::new(search));
//! }
//! if let Ok(crawl) = WebCrawlTool::from_env() {
//!     agent = agent.tool_arc(Arc::new(crawl));
//! }
//! # let _ = agent; Ok(()) }
//! ```

mod crawl;
mod fetch;
mod firecrawl;
mod policy;
mod search;

pub use crawl::WebCrawlTool;
pub use fetch::WebFetchTool;
pub use firecrawl::Firecrawl;
pub use policy::UrlPolicy;
pub use search::{SearchHit, SearchProvider, WebSearchTool};
