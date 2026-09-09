//! Minimal SDK example: OpenAI-compatible gateways.
//!
//! Vercel AI Gateway and CheaperInference speak the OpenAI Chat Completions
//! protocol, so they reuse `OpenAiModel` with a different base URL. Only the
//! catalog helpers live in their own provider modules.
//!
//! ```bash
//! GATEWAY_API_KEY=... cargo run -p orca-harness-sdk --example provider_gateway
//! ```

use orca_harness_model_providers::cheaperinference::CHEAPERINFERENCE_BASE_URL;
use orca_harness_model_providers::vercel::VERCEL_GATEWAY_BASE_URL;
use orca_harness_sdk::{Harness, OpenAiModel, RunRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key =
        std::env::var("GATEWAY_API_KEY").map_err(|_| "set GATEWAY_API_KEY to run this example")?;

    // Pick a gateway: `vercel` (default) or `cheaperinference`.
    let (base_url, model_id) = match std::env::var("GATEWAY").as_deref() {
        Ok("cheaperinference") => (CHEAPERINFERENCE_BASE_URL, "anthropic/claude-sonnet-4.5"),
        _ => (VERCEL_GATEWAY_BASE_URL, "anthropic/claude-sonnet-4.5"),
    };
    println!("Gateway: {base_url}");

    let model = OpenAiModel::new(model_id)
        .base_url(base_url)
        .api_key(api_key)
        .max_tokens(1024);

    let harness = Harness::builder()
        .workspace(std::env::current_dir()?)
        .build()?;

    let agent = harness
        .agent(model)
        .name("gateway-assistant")
        .system_prompt("You are a concise assistant. Answer in at most three sentences.")
        .usage(true)
        .build()?;

    let session = agent.new_session().open()?;
    let result = session
        .run(RunRequest::new(
            "In one paragraph, what is an agent execution kernel?",
        ))
        .await?;

    println!("Response: {}", result.text);
    println!(
        "Usage: {} input, {} output",
        result.usage.input_tokens, result.usage.output_tokens
    );
    Ok(())
}
