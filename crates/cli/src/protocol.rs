//! CLI parsing and persistence for transport selection. Spellings live on `Protocol`.
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

pub(crate) mod optional {
    use super::*;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        value: &Option<Protocol>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.map(Protocol::name).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Protocol>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .as_deref()
            .map(parse)
            .transpose()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_choices_roundtrip_and_reject_unknown_values() {
        for &protocol in Protocol::ALL {
            assert_eq!(parse(protocol.name()).unwrap(), protocol);
            let route = crate::config::SubagentModelSelection {
                provider: "local".into(),
                model: "fixture".into(),
                base_url: "http://localhost/v1".into(),
                automatic_base_url: false,
                protocol: Some(protocol),
            };
            let value = serde_json::to_value(&route).unwrap();
            assert_eq!(value["protocol"], protocol.name());
            assert_eq!(
                serde_json::from_value::<crate::config::SubagentModelSelection>(value).unwrap(),
                route
            );
        }
        assert!(parse("openai").is_err());
        assert!(parse("").is_err());
    }
}
