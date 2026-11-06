//! `--list-providers` and `--list-models --json`: machine-readable views of
//! the provider registry and one provider's catalog. Nothing here prints or
//! returns a key: `configured` only says whether one would be found.

use orca_harness_model_providers::{ModelInfo, SupportedEfforts};
use serde_json::{json, Value};

use crate::msg::{Provider, ProviderAuth};

/// What `--list-providers` reports for one provider. Kept separate from the
/// lookups so the document shape is testable without a home directory.
pub struct ProviderRow {
    pub provider: Provider,
    pub configured: bool,
    pub model: Option<String>,
}

impl ProviderRow {
    /// Probe the environment, the config file, and stored logins. Local
    /// files only: no network call is made.
    pub fn probe(provider: Provider) -> Self {
        let configured = match provider.auth() {
            ProviderAuth::ApiKey { .. } => provider.resolve_key().is_some(),
            ProviderAuth::OAuth => crate::auth::status(provider).is_ok(),
            ProviderAuth::None => true,
        };
        let model = crate::config::stored_model(provider.label())
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| provider.default_model().to_string());
        Self {
            provider,
            configured,
            model: Some(model),
        }
    }
}

pub fn auth_label(auth: ProviderAuth) -> &'static str {
    match auth {
        ProviderAuth::ApiKey { .. } => "api_key",
        ProviderAuth::OAuth => "oauth",
        ProviderAuth::None => "none",
    }
}

pub fn providers_document(active: Provider, rows: &[ProviderRow]) -> Value {
    json!({
        "active": active.label(),
        "providers": rows.iter().map(|row| json!({
            "label": row.provider.label(),
            "name": row.provider.display_name(),
            "auth": auth_label(row.provider.auth()),
            "configured": row.configured,
            "model": row.model,
        })).collect::<Vec<_>>(),
    })
}

pub fn providers_text(active: Provider, rows: &[ProviderRow]) -> String {
    rows.iter()
        .map(|row| {
            format!(
                "{} {} · {} · {} · {} · {}",
                if row.provider == active { "*" } else { " " },
                row.provider.label(),
                row.provider.display_name(),
                auth_label(row.provider.auth()),
                if row.configured {
                    "configured"
                } else {
                    "not configured"
                },
                row.model.as_deref().unwrap_or("-"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Efforts follow the TUI effort picker: only an explicit list is offered.
fn efforts(model: &ModelInfo) -> Option<Vec<String>> {
    match model.reasoning.as_ref()?.supported_efforts.as_ref()? {
        SupportedEfforts::Listed(efforts) if !efforts.is_empty() => Some(efforts.clone()),
        _ => None,
    }
}

pub fn models_document(provider: Provider, models: &[ModelInfo]) -> Value {
    json!({
        "provider": provider.label(),
        "models": models.iter().map(|model| json!({
            "id": model.id,
            "name": model.name,
            "contextWindow": model.context_length,
            "efforts": efforts(model),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(value: Value) -> ModelInfo {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn providers_document_lists_every_provider_with_auth_and_model() {
        let rows = Provider::ALL
            .iter()
            .map(|&provider| ProviderRow {
                provider,
                configured: provider == Provider::Local,
                model: Some(provider.default_model().into()),
            })
            .collect::<Vec<_>>();
        let doc = providers_document(Provider::Vercel, &rows);
        assert_eq!(doc["active"], "vercel");
        let providers = doc["providers"].as_array().unwrap();
        assert_eq!(providers.len(), Provider::ALL.len());
        let by = |label: &str| {
            providers
                .iter()
                .find(|p| p["label"] == label)
                .unwrap()
                .clone()
        };
        assert_eq!(by("openrouter")["auth"], "api_key");
        assert_eq!(by("openai-codex")["auth"], "oauth");
        assert_eq!(by("local")["auth"], "none");
        assert_eq!(by("local")["configured"], true);
        assert_eq!(by("openrouter")["name"], "OpenRouter");
        assert_eq!(by("anthropic")["model"], "claude-haiku-4-5");
        for provider in providers {
            let keys = provider.as_object().unwrap().keys().collect::<Vec<_>>();
            assert_eq!(keys, ["auth", "configured", "label", "model", "name"]);
        }
        assert!(providers_text(Provider::Vercel, &rows).contains("* vercel"));
    }

    #[test]
    fn models_document_maps_names_windows_and_listed_efforts() {
        let models = [
            model(
                json!({"id": "a", "name": "Model A", "context_length": 200000,
                "reasoning": {"supported_efforts": ["low", "high"]}}),
            ),
            model(json!({"id": "b"})),
            model(json!({"id": "c", "reasoning": {"supported_efforts": null}})),
        ];
        let doc = models_document(Provider::OpenRouter, &models);
        assert_eq!(
            doc,
            json!({"provider": "openrouter", "models": [
                {"id": "a", "name": "Model A", "contextWindow": 200000, "efforts": ["low", "high"]},
                {"id": "b", "name": null, "contextWindow": null, "efforts": null},
                {"id": "c", "name": null, "contextWindow": null, "efforts": null},
            ]})
        );
    }
}
