//! CLI parsing for transport selection. Spellings live on `Protocol`.
use crate::Provider;
use orca_harness_model_providers::registry::Protocol;

pub(crate) fn parse(value: &str) -> Result<Protocol, String> {
    Protocol::from_name(value).ok_or_else(|| {
        format!(
            "unknown protocol {value:?}; expected {}",
            Protocol::ALL
                .iter()
                .map(|protocol| protocol.name())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// The transport a route uses: an explicit choice, else the preset's own.
pub(crate) fn route(provider: Provider, explicit: Option<Protocol>) -> Option<Protocol> {
    explicit.or(provider.spec().protocol)
}
