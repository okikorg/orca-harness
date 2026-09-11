//! Minimal SDK example: run an agent against the OpenAI Codex backend.
//!
//! Codex uses ChatGPT account credentials rather than a plain API key, so the
//! host supplies a `CodexCredentialSource`. This example reads an access token
//! and account id from the environment; a real host would refresh them.
//!
//! ```bash
//! CODEX_ACCESS_TOKEN=... CODEX_ACCOUNT_ID=... \
//!   cargo run -p orca-harness-sdk --example provider_codex
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use orca_harness_sdk::{
    BearerCredential, CodexCredential, CodexCredentialSource, CredentialError, CredentialSource,
    Harness, OpenAiCodexModel, RunRequest,
};

struct EnvCodexCredentials {
    access_token: String,
    account_id: String,
}

#[async_trait]
impl CredentialSource for EnvCodexCredentials {
    async fn credential(&self) -> Result<BearerCredential, CredentialError> {
        Ok(BearerCredential {
            access_token: self.access_token.clone(),
            expires_at: None,
        })
    }

    async fn refresh(&self) -> Result<BearerCredential, CredentialError> {
        // A real host would exchange the refresh token here.
        self.credential().await
    }
}

#[async_trait]
impl CodexCredentialSource for EnvCodexCredentials {
    async fn codex_credential(&self) -> Result<CodexCredential, CredentialError> {
        Ok(CodexCredential {
            bearer: self.credential().await?,
            account_id: self.account_id.clone(),
        })
    }

    async fn refresh_codex(&self, _rejected: &str) -> Result<CodexCredential, CredentialError> {
        self.codex_credential().await
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let credentials = Arc::new(EnvCodexCredentials {
        access_token: std::env::var("CODEX_ACCESS_TOKEN")
            .map_err(|_| "set CODEX_ACCESS_TOKEN to run this example")?,
        account_id: std::env::var("CODEX_ACCOUNT_ID")
            .map_err(|_| "set CODEX_ACCOUNT_ID to run this example")?,
    });

    let model = OpenAiCodexModel::new("gpt-5-codex", credentials).reasoning_effort("low");

    let harness = Harness::builder()
        .workspace(std::env::current_dir()?)
        .build()?;

    let agent = harness
        .agent(model)
        .name("codex-assistant")
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
