//! Session-local sharing for endpoints that draw on the same provider quota.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use orca_harness_extensions::ModelGate;

use crate::Endpoint;

type QuotaKey = (String, String, Option<String>);

#[derive(Clone, Default)]
pub(crate) struct ModelGates(Arc<Mutex<HashMap<QuotaKey, ModelGate>>>);

impl ModelGates {
    pub(crate) fn for_endpoint(&self, endpoint: &Endpoint) -> ModelGate {
        // Deliberately share across model IDs: many providers pool their quota.
        let key = (
            endpoint.provider.id().to_owned(),
            endpoint.base_url.trim_end_matches('/').to_owned(),
            endpoint.api_key.clone(),
        );
        self.0
            .lock()
            .unwrap()
            .entry(key)
            .or_insert_with(|| {
                ModelGate::from_limit(endpoint.subagent_settings.model_concurrency_watch())
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_clones_and_models_share_quota_but_credentials_and_hosts_do_not() {
        let mut endpoint = Endpoint {
            api_key: Some("first-key".into()),
            model: "first-model".into(),
            ..Endpoint::fixture(crate::Provider::OpenAi, "https://example.test/v1/")
        };
        let registry = endpoint.model_gates.clone();
        let _first = registry.for_endpoint(&endpoint);
        let mut worker = endpoint.clone();
        worker.model = "second-model".into();
        worker.base_url.pop();
        let _second = worker.model_gates.for_endpoint(&worker);
        assert_eq!(registry.0.lock().unwrap().len(), 1);
        endpoint.api_key = Some("second-key".into());
        let _third = registry.for_endpoint(&endpoint);
        endpoint.base_url = "https://another.test/v1".into();
        let _fourth = registry.for_endpoint(&endpoint);
        assert_eq!(registry.0.lock().unwrap().len(), 3);
    }
}
