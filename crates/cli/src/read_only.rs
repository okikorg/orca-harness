//! `--read-only`: a headless run that changes nothing. Stricter than
//! `--plan`, which still writes markdown under `docs/plan/`.
//!
//! An allowlist, like plan mode: the observing tools, MCP tools (the caller
//! chose those servers for this run), and `shell` only when every command
//! goes to the caller through `--approvals stdio`. Everything else is
//! denied, including `subagent` and `workflow`, whose children would not
//! carry this gate. Registered before auto-approval so neither
//! `--auto-approve` nor `--yolo` can widen it.

use std::collections::HashSet;

use async_trait::async_trait;
use orca_harness_core::{Extension, ExtensionError, Subscriptions, ToolCall, ToolDecision};

use crate::mode::READ_ONLY_TOOLS;

pub struct ReadOnlyGate {
    allowed: HashSet<String>,
    reviewed_shell: bool,
}

impl ReadOnlyGate {
    /// `mcp_tools`: every tool name the run's MCP servers registered.
    /// `reviewed_shell`: shell commands each go to a stdio approval.
    pub fn new(mcp_tools: impl IntoIterator<Item = String>, reviewed_shell: bool) -> Self {
        let mut allowed = READ_ONLY_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .collect::<HashSet<_>>();
        allowed.extend(mcp_tools);
        Self {
            allowed,
            reviewed_shell,
        }
    }
}

#[async_trait]
impl Extension for ReadOnlyGate {
    fn name(&self) -> &str {
        "read-only"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_tool()
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        if self.allowed.contains(&call.name) || (self.reviewed_shell && call.name == "shell") {
            return Ok(ToolDecision::Continue);
        }
        let shell_hint = if call.name == "shell" {
            " Shell runs in read-only mode only when the caller reviews each command (--approvals stdio)."
        } else {
            ""
        };
        Ok(ToolDecision::Deny {
            reason: format!(
                "Read-only mode is on, so `{}` was not run: nothing may change files or \
                 state. Investigate with read_file, grep and glob and answer with what you \
                 found. Do not retry this call.{shell_hint}",
                call.name
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decide(gate: &ReadOnlyGate, name: &str) -> bool {
        let call = ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: json!({"path": "docs/plan/x.md", "content": "x"}),
        };
        let decision = futures_util::FutureExt::now_or_never(gate.before_tool(&call))
            .unwrap()
            .unwrap();
        matches!(decision, ToolDecision::Continue)
    }

    #[test]
    fn writes_compute_delegation_and_memory_writes_are_denied_everywhere() {
        let gate = ReadOnlyGate::new(["mcp__docs__search".to_string()], true);
        for name in [
            "write_file",
            "edit_file",
            "process",
            "pykernel",
            "bun_repl",
            "subagent",
            "workflow",
            "memory_manage",
            "unknown_tool",
        ] {
            assert!(!decide(&gate, name), "{name} must be denied");
        }
        for name in [
            "read_file",
            "grep",
            "glob",
            "web_fetch",
            "mcp__docs__search",
        ] {
            assert!(decide(&gate, name), "{name} must pass");
        }
    }

    #[test]
    fn shell_passes_only_to_a_reviewing_caller() {
        assert!(decide(&ReadOnlyGate::new([], true), "shell"));
        assert!(!decide(&ReadOnlyGate::new([], false), "shell"));
    }
}
