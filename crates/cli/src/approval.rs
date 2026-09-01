//! Interactive tool approval: a `before_tool` extension that pauses gated
//! tool calls until the user answers in the UI.

use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use orca_harness_core::{Extension, ExtensionError, Subscriptions, ToolCall, ToolDecision};
use tokio::sync::{mpsc, oneshot};

use crate::mode::ModeHandle;
use crate::msg::{ApprovalRequest, ApprovalResponse, UiMsg};
use crate::presentation;

/// Tools that mutate the machine or egress to arbitrary hosts and
/// therefore need a human answer. `web_search`/`web_crawl` are not here:
/// they only talk to the Firecrawl endpoint the user opted into by
/// providing a key.
pub const GATED_TOOLS: &[&str] = &[
    "shell",
    "write_file",
    "edit_file",
    "apply_patch",
    "multi_edit",
    "web_fetch",
    "pykernel",
    "bun_repl",
    "subagent",
    "memory_manage",
];

pub struct Approval {
    gated: HashSet<String>,
    always: Mutex<HashSet<String>>,
    /// Canonicalized workspace root: the key under which the user's saved
    /// (cross-session) always-allows live in the config file.
    workspace: String,
    /// The session's live mode. In yolo every gate below is skipped —
    /// the handle is read per call, so `/mode` applies to the tool call
    /// in flight, exactly like the plan gate.
    mode: ModeHandle,
    ui: mpsc::UnboundedSender<UiMsg>,
}

impl Approval {
    pub fn with_mode(
        mode: ModeHandle,
        ui: mpsc::UnboundedSender<UiMsg>,
        workspace: String,
    ) -> Self {
        Self {
            gated: GATED_TOOLS.iter().map(|s| s.to_string()).collect(),
            always: Mutex::new(HashSet::new()),
            workspace,
            mode,
            ui,
        }
    }

    /// Session grants, then the config file's per-workspace allowlist.
    /// The file is consulted per call, not cached: a revocation in
    /// /settings applies to the very next tool call, and a failed read
    /// fails closed (the prompt is shown).
    fn always_allowed(&self, tool: &str) -> bool {
        self.always.lock().unwrap().contains(tool)
            || crate::config::stored_approvals(&self.workspace)
                .iter()
                .any(|t| t == tool)
    }
}

#[async_trait]
impl Extension for Approval {
    fn name(&self) -> &str {
        "approval"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_tool()
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        // Auto has its own final-input reviewer and yolo intentionally has
        // no gate. Neither mode should also open the ordinary human prompt.
        if self.mode.get().bypasses_human_approval() {
            return Ok(ToolDecision::Continue);
        }
        if !self.gated.contains(&call.name) || self.always_allowed(&call.name) {
            return Ok(ToolDecision::Continue);
        }

        let (respond, answer) = oneshot::channel();
        let request = ApprovalRequest {
            tool_name: call.name.clone(),
            detail: presentation::tool_call_line(&call.name, &call.arguments),
            yes_no: false,
            respond,
        };
        let deny = || ToolDecision::Deny {
            reason: "The user denied this tool call.".into(),
        };
        if self.ui.send(UiMsg::Approval(request)).is_err() {
            return Ok(deny());
        }
        Ok(match answer.await {
            Ok(ApprovalResponse::AllowOnce) => ToolDecision::Continue,
            // The UI owns writing AllowAlwaysSave to the config (it can
            // report a failed write); here both grants act the same.
            Ok(ApprovalResponse::AllowAlways) | Ok(ApprovalResponse::AllowAlwaysSave) => {
                self.always.lock().unwrap().insert(call.name.clone());
                ToolDecision::Continue
            }
            Ok(ApprovalResponse::Deny) | Err(_) => deny(),
        })
    }
}

/// Headless runs have nobody to ask: gated tools are denied unless the
/// user opted in with `--auto-approve`.
pub struct HeadlessGate;

#[async_trait]
impl Extension for HeadlessGate {
    fn name(&self) -> &str {
        "headless-gate"
    }

    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::none().before_tool()
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision, ExtensionError> {
        if GATED_TOOLS.contains(&call.name.as_str()) {
            return Ok(ToolDecision::Deny {
                reason: format!(
                    "'{}' needs interactive approval; re-run orcacode with --auto-approve to allow it in headless mode.",
                    call.name
                ),
            });
        }
        Ok(ToolDecision::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::Mode;
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: json!({"command": "ls"}),
        }
    }

    fn denied(decision: &ToolDecision) -> bool {
        matches!(decision, ToolDecision::Deny { .. })
    }

    #[tokio::test]
    async fn ungated_tools_pass_without_asking() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approval = Approval::with_mode(ModeHandle::default(), tx, "/test-ws".into());
        let decision = approval.before_tool(&call("read_file")).await.unwrap();
        assert!(matches!(decision, ToolDecision::Continue));
        assert!(rx.try_recv().is_err(), "no approval request expected");
    }

    #[tokio::test]
    async fn gated_tool_waits_for_allow_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approval = Approval::with_mode(ModeHandle::default(), tx, "/test-ws".into());
        let answer = tokio::spawn(async move {
            match rx.recv().await {
                Some(UiMsg::Approval(req)) => {
                    assert_eq!(req.tool_name, "shell");
                    req.respond.send(ApprovalResponse::AllowOnce).unwrap();
                }
                _ => panic!("expected approval request"),
            }
        });
        let decision = approval.before_tool(&call("shell")).await.unwrap();
        assert!(matches!(decision, ToolDecision::Continue));
        answer.await.unwrap();
    }

    #[tokio::test]
    async fn allow_always_stops_asking_for_that_tool() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approval = Approval::with_mode(ModeHandle::default(), tx, "/test-ws".into());
        let answer = tokio::spawn(async move {
            match rx.recv().await {
                Some(UiMsg::Approval(req)) => {
                    req.respond.send(ApprovalResponse::AllowAlways).unwrap()
                }
                _ => panic!("expected approval request"),
            }
        });
        let first = approval.before_tool(&call("shell")).await.unwrap();
        assert!(matches!(first, ToolDecision::Continue));
        answer.await.unwrap();
        // Second call must continue without any receiver answering.
        let second = approval.before_tool(&call("shell")).await.unwrap();
        assert!(matches!(second, ToolDecision::Continue));
    }

    #[tokio::test]
    async fn deny_and_dropped_ui_both_deny() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approval = Approval::with_mode(ModeHandle::default(), tx, "/test-ws".into());
        let answer = tokio::spawn(async move {
            match rx.recv().await {
                Some(UiMsg::Approval(req)) => req.respond.send(ApprovalResponse::Deny).unwrap(),
                _ => panic!("expected approval request"),
            }
        });
        let decision = approval.before_tool(&call("shell")).await.unwrap();
        assert!(denied(&decision));
        answer.await.unwrap();

        // UI channel closed entirely: deny rather than hang or allow.
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let approval = Approval::with_mode(ModeHandle::default(), tx, "/test-ws".into());
        let decision = approval.before_tool(&call("write_file")).await.unwrap();
        assert!(denied(&decision));
    }

    #[tokio::test]
    async fn saved_workspace_approval_skips_the_prompt() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        crate::config::save_approval("/test-ws", "shell").unwrap();
        let approval = Approval::with_mode(ModeHandle::default(), tx, "/test-ws".into());

        let decision = approval.before_tool(&call("shell")).await.unwrap();
        assert!(matches!(decision, ToolDecision::Continue));
        assert!(rx.try_recv().is_err(), "saved approval must not prompt");

        // Revocation applies to the very next call: with no UI answering,
        // the pending prompt resolves as a deny when the request drops.
        crate::config::remove_approval("/test-ws", "shell").unwrap();
        let handle = tokio::spawn(async move {
            match rx.recv().await {
                Some(UiMsg::Approval(req)) => drop(req),
                _ => panic!("expected approval request after revocation"),
            }
        });
        let decision = approval.before_tool(&call("shell")).await.unwrap();
        assert!(denied(&decision), "revoked tool must prompt again");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn saved_approvals_do_not_leak_across_workspaces() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        crate::config::save_approval("/other-ws", "shell").unwrap();
        let approval = Approval::with_mode(ModeHandle::default(), tx, "/test-ws".into());

        let handle = tokio::spawn(async move {
            match rx.recv().await {
                Some(UiMsg::Approval(req)) => drop(req),
                _ => panic!("expected approval request"),
            }
        });
        let decision = approval.before_tool(&call("shell")).await.unwrap();
        assert!(
            denied(&decision),
            "another workspace's trust must not apply"
        );
        handle.await.unwrap();
    }

    #[test]
    fn mutations_compute_subagent_and_memory_writes_are_gated() {
        assert!(GATED_TOOLS.contains(&"apply_patch"));
        assert!(GATED_TOOLS.contains(&"multi_edit"));
        assert!(GATED_TOOLS.contains(&"pykernel"));
        assert!(GATED_TOOLS.contains(&"bun_repl"));
        assert!(GATED_TOOLS.contains(&"subagent"));
        assert!(GATED_TOOLS.contains(&"memory_manage"));
    }

    /// In yolo no gated tool prompts, whatever the session or saved
    /// approvals say — the mode outranks everything below it.
    #[tokio::test]
    async fn yolo_mode_prompts_for_nothing() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        crate::config::save_approval("/test-ws", "shell").unwrap();
        let mode = ModeHandle::new(Mode::Yolo);
        let approval = Approval::with_mode(mode.clone(), tx, "/test-ws".into());

        for name in GATED_TOOLS {
            let decision = approval.before_tool(&call(name)).await.unwrap();
            assert!(
                matches!(decision, ToolDecision::Continue),
                "{name} must not prompt in yolo"
            );
        }
        // Not even one request reached the UI.
        assert!(rx.try_recv().is_err(), "yolo must never prompt");
        crate::config::remove_approval("/test-ws", "shell").unwrap();

        // Live: flipping back to normal restores the ask on the next
        // call — nothing was rebuilt, nothing was cached open.
        mode.set(Mode::Normal);
        let handle = tokio::spawn(async move {
            match rx.recv().await {
                Some(UiMsg::Approval(req)) => drop(req),
                _ => panic!("expected approval request after leaving yolo"),
            }
        });
        let decision = approval.before_tool(&call("shell")).await.unwrap();
        assert!(denied(&decision), "leaving yolo must restore the prompt");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn auto_mode_leaves_gated_calls_to_the_automatic_reviewer() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approval = Approval::with_mode(ModeHandle::new(Mode::Auto), tx, "/test-ws".into());
        let decision = approval.before_tool(&call("shell")).await.unwrap();
        assert!(matches!(decision, ToolDecision::Continue));
        assert!(rx.try_recv().is_err(), "auto must not open a human prompt");
    }

    /// Plan and normal modes keep prompting. Auto is covered separately
    /// above; in plan mode the plan gate denies mutations before this hook.
    #[tokio::test]
    async fn non_yolo_modes_still_prompt() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approval = Approval::with_mode(ModeHandle::new(Mode::Plan), tx, "/test-ws".into());
        let answerer = tokio::spawn(async move {
            match rx.recv().await {
                Some(UiMsg::Approval(req)) => drop(req),
                _ => panic!("plan mode must still prompt gated tools"),
            }
        });
        let decision = approval.before_tool(&call("shell")).await.unwrap();
        assert!(denied(&decision));
        answerer.await.unwrap();

        // Normal mode behaves exactly as before.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approval = Approval::with_mode(ModeHandle::new(Mode::Normal), tx, "/test-ws".into());
        let answerer = tokio::spawn(async move {
            match rx.recv().await {
                Some(UiMsg::Approval(req)) => drop(req),
                _ => panic!("normal mode must still prompt gated tools"),
            }
        });
        let decision = approval.before_tool(&call("shell")).await.unwrap();
        assert!(denied(&decision));
        answerer.await.unwrap();
    }
}
