//! Live streaming smoke test against an OpenAI-compatible server.
//! Prints deltas as they arrive, then the authoritative final response.
//!
//! ```bash
//! cargo run -p orca-harness-model-openai --example stream_probe -- llama3.2:1b "why is the sky blue?"
//! ```
//!
//! Defaults to a local Ollama server; set `ORCA_BASE_URL` / `OPENAI_API_KEY`
//! to point elsewhere.

use std::io::Write;

use orca_harness_core::{Context, Model, ModelDelta, ModelResponse};
use orca_harness_model_openai::OpenAiModel;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let model_name = args.next().unwrap_or_else(|| "llama3.2:1b".into());
    let prompt = args
        .next()
        .unwrap_or_else(|| "In one sentence: why is the sky blue?".into());
    let base_url =
        std::env::var("ORCA_BASE_URL").unwrap_or_else(|_| "http://localhost:11434/v1".into());

    let mut model = OpenAiModel::new(&model_name).base_url(base_url);
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        model = model.api_key(key);
    }

    let mut context = Context::new();
    context.push_user(prompt);

    let sink = |delta: ModelDelta| {
        match delta {
            ModelDelta::Text { text } => print!("{text}"),
            ModelDelta::Reasoning { text } => eprint!("{text}"),
        }
        std::io::stdout().flush().ok();
        std::io::stderr().flush().ok();
    };

    let response = model.generate_streaming(&context, &[], &sink).await?;
    println!();
    match response {
        ModelResponse::Final { text, usage } => {
            println!("--- final ({} chars, usage: {usage:?})", text.len());
        }
        ModelResponse::ToolCalls { calls, .. } => {
            println!("--- tool calls: {calls:?}");
        }
    }
    Ok(())
}
