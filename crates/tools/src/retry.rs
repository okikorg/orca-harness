//! Retry classifiers for the built-in tools, shared by every host that
//! registers a retry layer (the SDK's tool retry, the `subagent` tool's
//! inner retry, and the CLI). They know the tools' output shapes and which
//! tools must never be replayed; they hold no retry logic themselves and
//! plug into `ToolRetry::retry_ok_when` / `retry_error_when` (or the
//! subagent tool's `retry_ok_when`) unchanged.

use orca_harness_core::{ToolCall, ToolError};
use serde_json::Value;

/// Tool names whose errors are never replayed: native file mutations.
const NON_IDEMPOTENT: &[&str] = &["write_file", "edit_file", "multi_edit", "apply_patch"];

/// The data-failure rule: an `Ok` result the core tools report as a
/// failure *in data*, so a retry layer covers the failures that happen
/// in practice:
/// - `shell` / `process` results carry `success: false`;
/// - `web_fetch` results carry an HTTP `status` (5xx retried, 4xx left
///   alone: the caller asked for something that is not there).
///
/// Every other tool's `Ok` is a success. Shared with the subagent relay so
/// inner agents retry exactly what the top-level agent retries.
pub fn data_failure(call: &ToolCall, out: &Value) -> bool {
    match call.name.as_str() {
        "shell" | "process" => out["success"].as_bool() == Some(false),
        "web_fetch" => match out["status"].as_u64() {
            Some(status) => (500..600).contains(&status),
            None => false,
        },
        _ => false,
    }
}

/// Returned-error retry policy. Native file mutations are never replayed:
/// their exact-match failures are deterministic, while retrying after an
/// I/O error could repeat an operation whose rollback was incomplete.
/// Subagent and workflow control actions (including unsupported legacy
/// wait calls) are deterministic too, and the poll-guard error on a
/// repeated `list` must reach the model, not be retried into a
/// harness-side poll loop. Other tools retain the retry extension's
/// historical retry-on-`Err` behavior, including a `subagent` or
/// `workflow` run (see [`excludes_delegation`] for the case where the
/// child retries on its own).
pub fn retryable_error(call: &ToolCall, _error: &ToolError) -> bool {
    if call.name == "subagent" {
        return call.arguments["action"]
            .as_str()
            .is_none_or(|action| action == "run");
    }
    if call.name == "workflow" {
        return call.arguments["action"].as_str() == Some("run");
    }
    !NON_IDEMPOTENT.contains(&call.name.as_str())
}

/// Whether `call` delegates work to a child agent: a `subagent` run (the
/// `action` absent or `run`) or a `workflow` run. A host whose children
/// retry their own tool calls excludes these from its parent-level retry,
/// otherwise a child's failure is replayed at both layers (`3 x 3`
/// attempts, each child re-spawned in full). Control actions (`list`,
/// `cancel`, `output`, ...) are not delegation.
pub fn excludes_delegation(call: &ToolCall) -> bool {
    match call.name.as_str() {
        "subagent" => call.arguments["action"]
            .as_str()
            .is_none_or(|action| action == "run"),
        "workflow" => call.arguments["action"].as_str() == Some("run"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "test".into(),
            name: name.into(),
            arguments,
        }
    }

    #[test]
    fn data_failures_are_shell_exits_and_server_errors() {
        assert!(data_failure(
            &call("shell", json!({})),
            &json!({"success": false})
        ));
        assert!(!data_failure(
            &call("shell", json!({})),
            &json!({"success": true})
        ));
        assert!(data_failure(
            &call("process", json!({})),
            &json!({"success": false})
        ));
        assert!(data_failure(
            &call("web_fetch", json!({})),
            &json!({"status": 503})
        ));
        assert!(!data_failure(
            &call("web_fetch", json!({})),
            &json!({"status": 404})
        ));
        assert!(!data_failure(
            &call("web_fetch", json!({})),
            &json!({"body": "x"})
        ));
        assert!(!data_failure(
            &call("grep", json!({})),
            &json!({"success": false})
        ));
        assert!(!data_failure(
            &call("subagent", json!({})),
            &json!({"success": false})
        ));
    }

    #[test]
    fn mutation_errors_are_not_retryable() {
        let error = ToolError::msg("deterministic");

        assert!(!retryable_error(&call("multi_edit", json!({})), &error));
        assert!(!retryable_error(&call("apply_patch", json!({})), &error));
        assert!(!retryable_error(&call("write_file", json!({})), &error));
        assert!(!retryable_error(&call("edit_file", json!({})), &error));
        assert!(retryable_error(&call("web_fetch", json!({})), &error));
        assert!(retryable_error(&call("grep", json!({})), &error));

        let control = |action: &str| call("subagent", json!({"action": action}));
        assert!(
            retryable_error(&call("subagent", json!({})), &error),
            "run by default"
        );
        assert!(retryable_error(&control("run"), &error));
        assert!(!retryable_error(&control("list"), &error));
        assert!(!retryable_error(&control("wait"), &error));
        assert!(!retryable_error(&control("cancel_all"), &error));
        assert!(retryable_error(
            &call("workflow", json!({"action": "run"})),
            &error
        ));
    }

    /// The poll-guard refusal on a repeated `workflow list` must reach the
    /// model instead of being replayed, exactly as for `subagent`.
    #[test]
    fn workflow_control_errors_are_not_retryable() {
        let error = ToolError::msg("workflow list unchanged; do not poll");
        let control = |action: &str| call("workflow", json!({"action": action}));

        assert!(!retryable_error(&control("list"), &error));
        assert!(!retryable_error(&control("cancel"), &error));
        assert!(!retryable_error(&control("output"), &error));
        assert!(
            !retryable_error(&call("workflow", json!({})), &error),
            "a missing action is a deterministic schema failure"
        );
        assert!(retryable_error(&control("run"), &error));
    }

    #[test]
    fn delegation_is_a_subagent_or_workflow_run() {
        assert!(excludes_delegation(&call("subagent", json!({"task": "x"}))));
        assert!(excludes_delegation(&call(
            "subagent",
            json!({"action": "run", "task": "x"})
        )));
        assert!(!excludes_delegation(&call(
            "subagent",
            json!({"action": "list"})
        )));
        assert!(!excludes_delegation(&call(
            "subagent",
            json!({"action": "cancel_all"})
        )));
        assert!(excludes_delegation(&call(
            "workflow",
            json!({"action": "run", "graph": []})
        )));
        assert!(!excludes_delegation(&call(
            "workflow",
            json!({"action": "list"})
        )));
        assert!(!excludes_delegation(&call("workflow", json!({}))));
        assert!(!excludes_delegation(&call("shell", json!({}))));
    }
}
