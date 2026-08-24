//! Provider-neutral host authentication dispatch.

mod oauth_device;
mod openai_codex;
mod store;

use crate::msg::Provider;
pub use openai_codex::CodexCliCredential;

pub fn status(provider: Provider) -> Result<&'static str, String> {
    match provider {
        Provider::OpenAiCodex => CodexCliCredential::discover()
            .status()
            .map_err(|error| error.message),
        _ => Err(format!("{} does not use OAuth", provider.label())),
    }
}

pub fn status_summary(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAiCodex => CodexCliCredential::discover().status_summary(),
        _ => "OAuth unavailable",
    }
}

pub async fn login(
    provider: Provider,
    progress: impl Fn(String) + Send + Sync + 'static,
) -> Result<(), String> {
    match provider {
        Provider::OpenAiCodex => openai_codex::login(progress)
            .await
            .map(|_| ())
            .map_err(|error| error.message),
        _ => Err(format!(
            "{} does not support interactive OAuth",
            provider.label()
        )),
    }
}
