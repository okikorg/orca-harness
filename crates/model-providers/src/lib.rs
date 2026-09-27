//! Model provider adapters for Orca Harness.
//!
//! Modules are organized by provider while protocol reuse stays internal:
//! OpenRouter delegates generation to the OpenAI-compatible adapter, and
//! Codex owns its distinct Responses protocol and credential lifecycle.

mod sse;

pub mod anthropic;
pub mod bedrock;
pub mod catalog;
pub mod cheaperinference;
pub mod copilot;
pub mod cursor;
mod discovery;
pub mod google;
pub mod http;
pub mod http_error;
pub mod openai;
pub mod openai_codex;
pub mod openrouter;
pub mod pi_messages;
mod provider;
pub mod registry;
pub mod responses;
pub mod tool_images;
pub mod vercel;

pub use anthropic::AnthropicModel;
pub use bedrock::BedrockModel;
pub use catalog::{ModelInfo, Pricing, ReasoningCapabilities, SupportedEfforts};
pub use copilot::CopilotModel;
pub use cursor::CursorModel;
pub use google::GoogleModel;
pub use openai::OpenAiModel;
pub use openai_codex::OpenAiCodexModel;
pub use openrouter::OpenRouterModel;
pub use pi_messages::PiMessagesModel;
pub use provider::{Attribution, ProviderModel};
pub use registry::{Protocol, ProviderPreset};
pub use responses::ResponsesModel;

/// The response every adapter builds at stream end: no calls is a final
/// answer, otherwise the calls with any non-empty accompanying text.
fn response(
    text: String,
    calls: Vec<orca_harness_core::ToolCall>,
    usage: Option<orca_harness_core::Usage>,
) -> orca_harness_core::ModelResponse {
    use orca_harness_core::ModelResponse;
    if calls.is_empty() {
        ModelResponse::Final { text, usage }
    } else {
        ModelResponse::ToolCalls {
            content: (!text.is_empty()).then_some(text),
            calls,
            usage,
        }
    }
}

fn image_data_url(image: &orca_harness_core::Image) -> String {
    format!("data:{};base64,{}", image.media_type, image.data)
}

#[cfg(test)]
mod request_bench;

#[cfg(test)]
mod test_server;

#[cfg(test)]
mod usage_reconciliation;
