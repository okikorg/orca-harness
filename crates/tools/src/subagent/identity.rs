//! Stable worker identity shared by tool results, listings, and notifications.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SubagentIdentity {
    pub provider: String,
    pub model: String,
    /// Stable tool-facing route such as `frontier/claude-sonnet-5`.
    /// `None` means the worker inherited the orchestrator model.
    pub route: Option<String>,
}

impl SubagentIdentity {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            route: None,
        }
    }

    pub(super) fn with_route(mut self, route: String) -> Self {
        self.route = Some(route);
        self
    }
}
