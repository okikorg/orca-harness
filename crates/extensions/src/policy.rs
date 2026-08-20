//! Tool policy extension: allow/deny tool calls in `before_tool` before
//! any execution starts. This is the guardrail seam — enforce an
//! allowlist, block dangerous tools, or plug in a custom predicate.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use orca_harness_core::{Extension, ExtensionError, Subscriptions, ToolCall, ToolDecision};

/// Decision a [`PolicyRule`] returns for one call.
pub enum PolicyOutcome {
    Allow,
    Deny(String),
}

/// A pluggable predicate over a tool call.
pub trait PolicyRule: Send + Sync {
    fn evaluate(&self, call: &ToolCall) -> PolicyOutcome;
}

impl<F> PolicyRule for F
where
    F: Fn(&ToolCall) -> PolicyOutcome + Send + Sync,
{
    fn evaluate(&self, call: &ToolCall) -> PolicyOutcome {
        self(call)
    }
}

/// Enforces a tool-name policy. Construct with an allowlist, a denylist,
/// or an arbitrary rule; the first denying check wins.
pub struct ToolPolicy {
    allow: Option<HashSet<String>>,
    deny: HashSet<String>,
    rule: Option<Arc<dyn PolicyRule>>,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolPolicy {
    pub fn new() -> Self {
        Self {
            allow: None,
            deny: HashSet::new(),
            rule: None,
        }
    }

    /// Only these tool names may run. An empty allowlist denies nothing
    /// (matching the platform convention where empty `tools` means "all");
    /// pass explicit names to restrict.
    pub fn allow<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let set: HashSet<String> = names.into_iter().map(Into::into).collect();
        if !set.is_empty() {
            self.allow = Some(set);
        }
        self
    }

    /// These tool names are always denied (takes precedence over allow).
    pub fn deny<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.deny.extend(names.into_iter().map(Into::into));
        self
    }

    /// A custom predicate evaluated after allow/deny lists pass.
    pub fn rule(mut self, rule: impl PolicyRule + 'static) -> Self {
        self.rule = Some(Arc::new(rule));
        self
    }
}

#[async_trait]
impl Extension for ToolPolicy {
    fn name(&self) -> &str {
        "tool-policy"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_tool()
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        if self.deny.contains(&call.name) {
            return Ok(ToolDecision::Deny {
                reason: format!("tool '{}' is denied by policy", call.name),
            });
        }
        if let Some(allow) = &self.allow {
            if !allow.contains(&call.name) {
                return Ok(ToolDecision::Deny {
                    reason: format!("tool '{}' is not in the allowlist", call.name),
                });
            }
        }
        if let Some(rule) = &self.rule {
            if let PolicyOutcome::Deny(reason) = rule.evaluate(call) {
                return Ok(ToolDecision::Deny { reason });
            }
        }
        Ok(ToolDecision::Continue)
    }
}
