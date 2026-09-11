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
        name: "long-session",
        description: "continue bounded interactive runs and compact near the active model's context limit",
        default_on: true,
    },
    ExtensionSpec {
        name: "truncation",
        description: "cap oversized tool outputs at 16k chars; read_tool_result pages the original",
        default_on: true,
    },
    ExtensionSpec {
        name: "retry",
        description: "retry eligible tool failures (3 attempts; excludes native file mutations and covers data failures like nonzero shell exits and HTTP 5xx)",
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
/// with a short backoff. Native mutation errors are excluded; beyond other
/// plain `Err` failures, also retry failures core tools report *as data*, so
/// the toggle actually covers the failures that happen in practice:
/// - `shell` / `process` results carry `success: false`;
/// - `web_fetch` results carry an HTTP `status` (5xx retried, 4xx left
///   alone — the caller asked for something that is not there).
///
/// When attempts are exhausted the last real output is returned as-is, so
/// the model still sees the actual failure instead of a synthetic one.
pub(crate) const TOOL_RETRY_ATTEMPTS: u32 = 3;
pub(crate) const TOOL_RETRY_BACKOFF_MS: u32 = 250;

pub fn tool_retry() -> orca_harness_extensions::ToolRetry {
    orca_harness_extensions::ToolRetry::new(TOOL_RETRY_ATTEMPTS)
        .backoff(std::time::Duration::from_millis(u64::from(
            TOOL_RETRY_BACKOFF_MS,
        )))
        .retry_ok_when(data_failure)
        .retry_error_when(retryable_error)
}

/// The classifiers are the tools crate's, so every host and the subagent
/// relay retry exactly the same failures; see
/// [`orca_harness_tools::retry`].
pub use orca_harness_tools::retry::{data_failure, retryable_error};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use orca_harness_core::testing::{call, ScriptedModel};
    use orca_harness_core::Agent;
    use orca_harness_extensions::{EventStream, HarnessEvent};
    use orca_harness_tools::{
        FileGuard, MultiEditTool, MutationPreflight, Workspace, WriteFileTool,
    };

    #[test]
    fn defaults_apply_until_an_override_is_saved() {
        assert!(enabled("long-session"), "long-session defaults on");
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

    #[tokio::test]
    async fn real_multi_edit_match_error_executes_once() {
        let root =
            std::env::temp_dir().join(format!("orca-multi-edit-no-retry-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("demo.txt");
        std::fs::write(&path, "same\nsame\nsame\nsame\n").unwrap();

        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = attempts.clone();
        let events = EventStream::from_fn(move |event| {
            if matches!(event, HarnessEvent::ToolStarted { .. }) {
                seen.fetch_add(1, Ordering::SeqCst);
            }
        });
        let execution_marker = events.execution_marker();
        let model = ScriptedModel::tool_round(
            vec![call(
                "ambiguous",
                "multi_edit",
                serde_json::json!({
                    "edits": [{"path": "demo.txt", "old": "same", "new": "changed"}]
                }),
            )],
            "done",
        );

        Agent::new(model)
            .tool(MultiEditTool::new(Workspace::new(root.clone())))
            .extension(events)
            .extension(MutationPreflight)
            .extension(tool_retry())
            .extension(execution_marker)
            .run("edit the file")
            .await
            .unwrap();

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "same\nsame\nsame\nsame\n"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn real_write_guard_error_executes_once() {
        let root =
            std::env::temp_dir().join(format!("orca-write-file-no-retry-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("demo.txt");
        std::fs::write(&path, "preserve me\n").unwrap();

        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = attempts.clone();
        let events = EventStream::from_fn(move |event| {
            if matches!(event, HarnessEvent::ToolStarted { .. }) {
                seen.fetch_add(1, Ordering::SeqCst);
            }
        });
        let execution_marker = events.execution_marker();
        let model = ScriptedModel::tool_round(
            vec![call(
                "overwrite",
                "write_file",
                serde_json::json!({"path": "demo.txt", "content": "replace me\n"}),
            )],
            "done",
        );

        Agent::new(model)
            .tool(WriteFileTool::new(Workspace::new(root.clone())).guard(FileGuard::new()))
            .extension(events)
            .extension(tool_retry())
            .extension(execution_marker)
            .run("overwrite without reading")
            .await
            .unwrap();

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "preserve me\n");
        std::fs::remove_dir_all(root).ok();
    }
}
