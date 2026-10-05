//! Model provider adapters for Orca Harness.
//!
//! Modules are organized by provider while protocol reuse stays internal:
//! OpenRouter delegates generation to the OpenAI-compatible adapter, and
//! Codex owns its distinct Responses protocol and credential lifecycle.

mod sse;

pub mod anthropic;
pub mod catalog;
pub mod cheaperinference;
pub mod http;
pub mod http_error;
pub mod openai;
pub mod openai_codex;
pub mod openrouter;
pub mod tool_images;
pub mod vercel;

pub use anthropic::AnthropicModel;
pub use catalog::{ModelInfo, Pricing, ReasoningCapabilities, SupportedEfforts};
pub use openai::OpenAiModel;
pub use openai_codex::OpenAiCodexModel;
pub use openrouter::OpenRouterModel;

/// The link a provider fetches for `image`, used only when it carries no
/// bytes of its own: an image with both is sent inline, so it never
/// depends on a link that may have expired.
fn image_link(image: &orca_harness_core::Image) -> Option<&str> {
    image
        .source_url
        .as_deref()
        .filter(|_| image.data.is_empty())
}

fn image_url(image: &orca_harness_core::Image) -> String {
    match image_link(image) {
        Some(url) => url.to_owned(),
        None => format!("data:{};base64,{}", image.media_type, image.data),
    }
}

#[cfg(test)]
mod request_bench;

#[cfg(test)]
mod usage_reconciliation;
