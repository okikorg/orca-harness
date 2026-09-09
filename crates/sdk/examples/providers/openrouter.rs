//! Minimal SDK example: run an agent against OpenRouter.
//!
//! ```bash
//! OPENROUTER_API_KEY=sk-or-... cargo run -p orca-harness-sdk --example provider_openrouter
//! ```

use orca_harness_sdk::{Harness, OpenRouterModel, RunRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| "set OPENROUTER_API_KEY to run this example")?;

    let model = OpenRouterModel::new("anthropic/claude-sonnet-4.5")
        .api_key(api_key)
        .title("Orca Harness SDK Example")
        .max_tokens(1024)
        .prompt_cache(true);

    let harness = Harness::builder()
        .workspace(std::env::current_dir()?)
        .build()?;

    let agent = harness
        .agent(model)
        .name("openrouter-assistant")
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
