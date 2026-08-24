//! Provider-neutral model catalog metadata exposed to hosts.

use serde::Deserialize;

/// One catalog entry. The optional fields are OpenRouter metadata; plain
/// OpenAI-compatible endpoints fill only `id`.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub pricing: Option<Pricing>,
}

/// USD per token, carried as decimal strings on the wire.
#[derive(Debug, Clone, Deserialize)]
pub struct Pricing {
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub completion: Option<String>,
}

impl ModelInfo {
    /// One display line: id, context window, price per million tokens.
    pub fn summary(&self) -> String {
        let mut out = self.id.clone();
        if let Some(ctx) = self.context_length {
            out.push_str(&format!("  {} ctx", ctx_label(ctx)));
        }
        let pricing = self.pricing.as_ref();
        let prompt = pricing.and_then(|p| per_million(p.prompt.as_deref()));
        let completion = pricing.and_then(|p| per_million(p.completion.as_deref()));
        match (prompt, completion) {
            (Some(p), Some(c)) if p == 0.0 && c == 0.0 => out.push_str("  free"),
            (Some(p), Some(c)) => out.push_str(&format!("  ${p:.2}/M in ${c:.2}/M out")),
            _ => {}
        }
        out
    }
}

fn ctx_label(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.0}M", tokens as f64 / 1e6)
    } else {
        format!("{}k", tokens / 1000)
    }
}

fn per_million(per_token: Option<&str>) -> Option<f64> {
    per_token?.trim().parse::<f64>().ok().map(|p| p * 1e6)
}
