//! Minimal SDK example: run an agent against the Anthropic provider.
//!
//! Requires `ANTHROPIC_API_KEY`. Run with:
//!
//! ```bash
//! ANTHROPIC_API_KEY=sk-ant-... cargo run -p orca-harness-sdk --example provider_anthropic
//! ```

use orca_harness_sdk::{AnthropicModel, Harness, RunRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| "set ANTHROPIC_API_KEY to run this example")?;

    // The provider adapter is the only provider-specific line in the example.
    let model = AnthropicModel::new("claude-sonnet-4-5")
        .api_key(api_key)
        .max_tokens(1024)
        .temperature(0.2)
        .prompt_cache(true);

    let harness = Harness::builder()
        .workspace(std::env::current_dir()?)
        .build()?;

    let agent = harness
        .agent(model)
        .name("anthropic-assistant")
        .system_prompt("You are a concise assistant. Answer in at most three sentences.")
        .usage(true)
        .build()?;

    // Runs happen through a session; `ephemeral()` keeps nothing on disk.
    let session = agent.new_session().open()?;
    let result = session
        .run(RunRequest::new(
            "In one paragraph, what is an agent execution kernel?",
        ))
        .await?;

    println!("Response: {}", result.text);
    println!(
        "Usage: {} input, {} output, {} cache read",
        result.usage.input_tokens, result.usage.output_tokens, result.usage.cache_read_tokens
    );
    Ok(())
}
