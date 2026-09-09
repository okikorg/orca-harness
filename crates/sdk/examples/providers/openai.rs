//! Minimal SDK example: run an agent against the OpenAI provider.
//!
//! ```bash
//! OPENAI_API_KEY=sk-... cargo run -p orca-harness-sdk --example provider_openai
//! ```

use orca_harness_sdk::{Harness, OpenAiModel, RunRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key =
        std::env::var("OPENAI_API_KEY").map_err(|_| "set OPENAI_API_KEY to run this example")?;

    let model = OpenAiModel::new("gpt-5")
        .api_key(api_key)
        .max_tokens(1024)
        .reasoning_effort("low")
        .usage_accounting(true);

    let harness = Harness::builder()
        .workspace(std::env::current_dir()?)
        .build()?;

    let agent = harness
        .agent(model)
        .name("openai-assistant")
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
