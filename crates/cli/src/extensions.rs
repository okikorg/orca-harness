//! The catalog of user-toggleable harness extensions. `/extensions`
//! lists these; enable/disable overrides persist in the config file and
//! the worker rebuilds the agent so a toggle applies to the next run.
//!
//! Extensions the TUI itself depends on (event streaming, approvals)
//! are not listed — disabling them would break the interface, so they
//! are always registered.

/// One user-toggleable extension.
pub struct ExtensionSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// State when the config file has no override saved.
    pub default_on: bool,
}

pub const EXTENSIONS: &[ExtensionSpec] = &[
    ExtensionSpec {
        name: "truncation",
        description: "cap oversized tool outputs at 16k chars; read_tool_result pages the original",
        default_on: true,
    },
    ExtensionSpec {
        name: "retry",
        description: "retry a failing tool call (3 attempts; covers Err failures and data failures like nonzero shell exits and HTTP 5xx)",
        default_on: false,
    },
];

pub fn find(name: &str) -> Option<&'static ExtensionSpec> {
    EXTENSIONS.iter().find(|spec| spec.name == name)
}

/// Effective state: the saved override, or the extension's default.
pub fn is_enabled(spec: &ExtensionSpec) -> bool {
    crate::config::stored_extension(spec.name).unwrap_or(spec.default_on)
}

/// [`is_enabled`] by name, for agent-build sites. Unknown names are
/// never enabled.
pub fn enabled(name: &str) -> bool {
    find(name).is_some_and(is_enabled)
}

/// The tool-retry extension as the CLI registers it: three total attempts
/// with a short backoff, and — beyond plain `Err` failures — also retry
/// the failures the core tools report *as data*, so the toggle actually
/// covers the failures that happen in practice:
/// - `shell` / `process` results carry `success: false`;
/// - `web_fetch` results carry an HTTP `status` (5xx retried, 4xx left
///   alone — the caller asked for something that is not there).
///
/// When attempts are exhausted the last real output is returned as-is, so
/// the model still sees the actual failure instead of a synthetic one.
pub fn tool_retry() -> orca_harness_extensions::ToolRetry {
    orca_harness_extensions::ToolRetry::new(3)
        .backoff(std::time::Duration::from_millis(250))
        .retry_ok_when(data_failure)
}

/// The data-failure rule for [`tool_retry`], shared with the subagent
/// relay so inner agents retry exactly what the top-level agent retries.
pub fn data_failure(call: &orca_harness_core::ToolCall, out: &serde_json::Value) -> bool {
    match call.name.as_str() {
        "shell" | "process" => out["success"].as_bool() == Some(false),
        "web_fetch" => match out["status"].as_u64() {
            Some(status) => (500..600).contains(&status),
            None => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_until_an_override_is_saved() {
        assert!(enabled("truncation"), "truncation defaults on");
        assert!(!enabled("retry"), "retry defaults off");

        crate::config::save_extension("truncation", false).unwrap();
        crate::config::save_extension("retry", true).unwrap();
        assert!(!enabled("truncation"));
        assert!(enabled("retry"));

        // Re-enabling restores the extension, not just the default.
        crate::config::save_extension("truncation", true).unwrap();
        assert!(enabled("truncation"));
    }

    #[test]
    fn unknown_names_are_never_enabled() {
        assert!(find("no-such").is_none());
        assert!(!enabled("no-such"));
    }
}
