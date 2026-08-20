//! Interactive tool approval: a `before_tool` extension that pauses gated
//! tool calls until the user answers in the UI.

use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use orca_harness_core::{Extension, ExtensionError, Subscriptions, ToolCall, ToolDecision};
use tokio::sync::{mpsc, oneshot};

use crate::msg::{ApprovalRequest, ApprovalResponse, UiMsg};
use crate::view;

/// Tools that mutate the machine or egress to arbitrary hosts and
/// therefore need a human answer. `web_search`/`web_crawl` are not here:
/// they only talk to the Firecrawl endpoint the user opted into by
/// providing a key.
pub const GATED_TOOLS: &[&str] = &[
    "shell",
    "write_file",
    "edit_file",
    "web_fetch",
    "kernel",
    "subagent",
];

pub struct Approval {
    gated: HashSet<String>,
    always: Mutex<HashSet<String>>,
    ui: mpsc::UnboundedSender<UiMsg>,
}

impl Approval {
    pub fn new(ui: mpsc::UnboundedSender<UiMsg>) -> Self {
        Self {
            gated: GATED_TOOLS.iter().map(|s| s.to_string()).collect(),
            always: Mutex::new(HashSet::new()),
            ui,
        }
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
        if !self.gated.contains(&call.name) || self.always.lock().unwrap().contains(&call.name) {
            return Ok(ToolDecision::Continue);
        }

        let (respond, answer) = oneshot::channel();
        let request = ApprovalRequest {
            tool_name: call.name.clone(),
            detail: view::tool_call_line(&call.name, &call.arguments),
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
            Ok(ApprovalResponse::AllowAlways) => {
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
                    "'{}' needs interactive approval; re-run orca with --auto-approve to allow it in headless mode.",
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
        let approval = Approval::new(tx);
        let decision = approval.before_tool(&call("read_file")).await.unwrap();
        assert!(matches!(decision, ToolDecision::Continue));
        assert!(rx.try_recv().is_err(), "no approval request expected");
    }

    #[tokio::test]
    async fn gated_tool_waits_for_allow_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approval = Approval::new(tx);
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
        let approval = Approval::new(tx);
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
        let approval = Approval::new(tx);
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
        let approval = Approval::new(tx);
        let decision = approval.before_tool(&call("write_file")).await.unwrap();
        assert!(denied(&decision));
    }

    #[test]
    fn kernel_and_subagent_are_gated() {
        assert!(GATED_TOOLS.contains(&"kernel"));
        assert!(GATED_TOOLS.contains(&"subagent"));
    }
}
