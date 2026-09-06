//! Persist tier assignments separately from live routing/governance settings.
use super::storage::{load_root, mutate_section};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SubagentModelSelection {
    pub provider: String,
    pub model: String,
    pub base_url: String,
}

pub(crate) fn stored_subagent_models() -> BTreeMap<String, SubagentModelSelection> {
    let Some(value) = load_root().and_then(|root| root.get("subagent_models").cloned()) else {
        return BTreeMap::new();
    };
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(tier, value)| {
            if !["local", "flash", "mid", "frontier"].contains(&tier.as_str()) {
                return None;
            }
            let selection: SubagentModelSelection = serde_json::from_value(value.clone()).ok()?;
            if crate::Provider::from_label(&selection.provider).is_none()
                || selection.model.trim().is_empty()
            {
                return None;
            }
            Some((tier.clone(), selection))
        })
        .collect()
}

pub(crate) fn save_subagent_model(
    tier: &str,
    selection: SubagentModelSelection,
) -> std::io::Result<std::path::PathBuf> {
    if !["local", "flash", "mid", "frontier"].contains(&tier) || selection.model.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid subagent model selection",
        ));
    }
    mutate_section("subagent_models", tier, move |entry| {
        *entry = serde_json::json!(selection)
    })
}
