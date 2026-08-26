//! Curated worker models offered to the orchestrator.
//!
//! Snapshot reviewed 2026-08-25 against OpenRouter's public model catalog,
//! Artificial Analysis, and the linked Ollama model cards. This is deliberately
//! static: startup stays deterministic and model availability errors stay
//! visible instead of silently rerouting.

use std::sync::Arc;

use orca_harness_core::Model;
use orca_harness_tools::SubagentModel;

use crate::{Endpoint, Provider};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CuratedModel {
    choice_id: &'static str,
    endpoint_model: &'static str,
    description: &'static str,
}

const LOCAL: [CuratedModel; 3] = [
    CuratedModel {
        choice_id: "local/qwen3-coder-next",
        endpoint_model: "qwen3-coder-next:latest",
        description: "local/free · strongest coding specialist here · ~52 GB · 256K context",
    },
    CuratedModel {
        choice_id: "local/glm-4.7-flash",
        endpoint_model: "glm-4.7-flash:latest",
        description: "local/free · efficient 30B-A3B coding agent · ~19 GB · 198K context",
    },
    CuratedModel {
        choice_id: "local/gpt-oss-20b",
        endpoint_model: "gpt-oss:20b",
        description: "local/free · compact general agent · ~14 GB · 128K context",
    },
];

const OPENROUTER: [CuratedModel; 9] = [
    CuratedModel {
        choice_id: "flash/gemini-3.7-flash",
        endpoint_model: "google/gemini-3.7-flash",
        description: "remote · fast premium worker · $0.375/M in, $1.875/M out · 1M context",
    },
    CuratedModel {
        choice_id: "flash/deepseek-v4-flash",
        endpoint_model: "deepseek/deepseek-v4-flash-0731",
        description: "remote · ultra-low-cost coding worker · $0.04/M in, $0.08/M out · 1M context",
    },
    CuratedModel {
        choice_id: "flash/qwen3.7-flash",
        endpoint_model: "qwen/qwen3.7-flash",
        description: "remote · cheap multimodal worker · $0.03/M in, $0.13/M out · 1M context",
    },
    CuratedModel {
        choice_id: "mid/kimi-k2.7-code",
        endpoint_model: "moonshotai/kimi-k2.7-code",
        description: "remote · coding-specific long-horizon worker · $0.67/M in, $3.40/M out",
    },
    CuratedModel {
        choice_id: "mid/glm-5.2",
        endpoint_model: "z-ai/glm-5.2",
        description: "remote · project-scale reasoning · $1.19/M in, $3.74/M out · 1M context",
    },
    CuratedModel {
        choice_id: "mid/minimax-m3",
        endpoint_model: "minimax/minimax-m3",
        description: "remote · economical multimodal agent · $0.30/M in, $1.20/M out",
    },
    CuratedModel {
        choice_id: "frontier/claude-opus-5",
        endpoint_model: "anthropic/claude-opus-5",
        description: "remote · maximum-confidence long-horizon coding · $5/M in, $25/M out",
    },
    CuratedModel {
        choice_id: "frontier/gpt-5.6-sol",
        endpoint_model: "openai/gpt-5.6-sol",
        description: "remote · strong command-line and multi-step coding · $2/M in, $10/M out",
    },
    CuratedModel {
        choice_id: "frontier/claude-sonnet-5",
        endpoint_model: "anthropic/claude-sonnet-5",
        description: "remote · frontier price/performance · $2/M in, $10/M out",
    },
];

/// Build choices reachable from this host. Local choices always target Ollama's
/// conventional endpoint; when the active endpoint itself is local, preserve
/// its configured URL. Cloud choices use OpenRouter and are included only when
/// an API key can be resolved without prompting.
pub(crate) fn choices(endpoint: &Endpoint) -> Vec<SubagentModel<Arc<dyn Model>>> {
    choices_with_openrouter_key(endpoint, Provider::OpenRouter.resolve_key())
}

fn choices_with_openrouter_key(
    endpoint: &Endpoint,
    openrouter_key: Option<String>,
) -> Vec<SubagentModel<Arc<dyn Model>>> {
    let local = Endpoint {
        provider: Provider::Local,
        base_url: if endpoint.provider == Provider::Local {
            endpoint.base_url.clone()
        } else {
            Provider::Local.base_url().into()
        },
        api_key: None,
        model: String::new(),
    };
    let mut choices = build(&local, &LOCAL);

    let openrouter_key = if endpoint.provider == Provider::OpenRouter {
        endpoint.api_key.clone().or(openrouter_key)
    } else {
        openrouter_key
    };
    if let Some(key) = openrouter_key {
        let openrouter = Endpoint {
            provider: Provider::OpenRouter,
            base_url: Provider::OpenRouter.base_url().into(),
            api_key: Some(key),
            model: String::new(),
        };
        choices.extend(build(&openrouter, &OPENROUTER));
    }
    choices
}

fn build(endpoint: &Endpoint, entries: &[CuratedModel]) -> Vec<SubagentModel<Arc<dyn Model>>> {
    entries
        .iter()
        .map(|entry| {
            let worker = Endpoint {
                model: entry.endpoint_model.into(),
                ..endpoint.clone()
            };
            SubagentModel::new(entry.choice_id, entry.description, worker.build_model())
                .identity(worker.provider.label(), worker.model)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortlist_has_three_unique_models_per_tier() {
        let all = LOCAL.into_iter().chain(OPENROUTER);
        let ids: std::collections::HashSet<_> = all.clone().map(|model| model.choice_id).collect();
        assert_eq!(ids.len(), 12);
        for tier in ["local/", "flash/", "mid/", "frontier/"] {
            assert_eq!(
                all.clone()
                    .filter(|model| model.choice_id.starts_with(tier))
                    .count(),
                3
            );
        }
        assert!(all.clone().all(|model| !model.endpoint_model.is_empty()));
        let choices = choices_with_openrouter_key(
            &Endpoint {
                provider: Provider::Local,
                base_url: Provider::Local.base_url().into(),
                api_key: None,
                model: "orchestrator".into(),
            },
            Some("key".into()),
        );
        assert!(choices.iter().all(|choice| choice.identity.is_some()));
        assert!(choices.iter().any(|choice| {
            let identity = choice.identity.as_ref().unwrap();
            identity.provider == "openrouter" && identity.model.contains('/')
        }));
        assert!(all
            .clone()
            .all(|model| !model.endpoint_model.ends_with(":batch")));
    }

    #[test]
    fn local_choices_exist_without_a_cloud_key() {
        let endpoint = Endpoint {
            provider: Provider::OpenAi,
            base_url: Provider::OpenAi.base_url().into(),
            api_key: Some("openai-key".into()),
            model: "gpt-4o".into(),
        };
        let choices = choices_with_openrouter_key(&endpoint, None);
        assert_eq!(choices.len(), 3);
        assert!(choices.iter().all(|choice| choice.id.starts_with("local/")));
    }

    #[test]
    fn openrouter_key_adds_all_three_remote_tiers() {
        let endpoint = Endpoint {
            provider: Provider::Local,
            base_url: Provider::Local.base_url().into(),
            api_key: None,
            model: "qwen3.5:9b".into(),
        };
        let choices = choices_with_openrouter_key(&endpoint, Some("openrouter-key".into()));
        assert_eq!(choices.len(), 12);
        for tier in ["flash/", "mid/", "frontier/"] {
            assert_eq!(
                choices
                    .iter()
                    .filter(|choice| choice.id.starts_with(tier))
                    .count(),
                3
            );
        }
    }
}
