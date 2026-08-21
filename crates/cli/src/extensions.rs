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
        description: "retry a failing tool call (3 attempts with a short backoff)",
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
