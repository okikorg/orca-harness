//! Incident triage agent: the on-call first responder.
//!
//! An alert arrives as free text. The agent classifies it against a
//! severity rubric, then pages the on-call rotation only when the
//! classification earns it. A tool policy enforces that rule on the host
//! side so a mis-classifying model cannot wake anyone at 3am for a sev3.
//!
//! The agent definition is the first thing in `main`: system prompt,
//! domain tools, and paging policy. Everything below `main` is scaffolding.
//!
//! Deterministic: the model is scripted and never calls a provider.

#[path = "../support/mod.rs"]
mod support;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{FnTool, ModelResponse, ToolCall};
use orca_harness_extensions::{HarnessEvent, PolicyOutcome, ToolPolicy};
use orca_harness_sdk::{Harness, RunRequest};
use serde_json::json;
use support::TempWorkspace;

const TRIAGE_PROMPT: &str = "\
You are the on-call triage responder. For every alert:
1. Call `classify_severity` with the alert text to get a severity and rubric reason.
2. Page the rotation with `page_oncall` only when the severity is sev1.
3. Answer with the severity, the reason, and whether anyone was paged.";

/// Applies the rubric. A real deployment would call an incident service;
/// here the rubric is a keyword table so the example stays hermetic.
fn classify_severity() -> FnTool {
    FnTool::new(
        "classify_severity",
        "Classify an alert as sev1, sev2, or sev3 against the on-call rubric.",
        json!({
            "type": "object",
            "properties": { "alert": { "type": "string" } },
            "required": ["alert"]
        }),
        |args, _ctx| async move {
            let alert = args["alert"].as_str().unwrap_or_default().to_lowercase();
            let (severity, reason) = if alert.contains("checkout") || alert.contains("outage") {
                ("sev1", "customer-facing revenue path is unavailable")
            } else if alert.contains("latency") || alert.contains("degraded") {
                ("sev2", "degraded but serving traffic")
            } else {
                ("sev3", "no customer impact observed")
            };
            Ok(json!({ "severity": severity, "reason": reason }))
        },
    )
}

/// Pages a rotation. The policy below decides whether the call is allowed
/// to reach this body at all.
fn page_oncall() -> FnTool {
    FnTool::new(
        "page_oncall",
        "Page the on-call rotation. Permitted for sev1 only.",
        json!({
            "type": "object",
            "properties": {
                "rotation": { "type": "string" },
                "severity": { "type": "string" },
                "summary": { "type": "string" }
            },
            "required": ["rotation", "severity", "summary"]
        }),
        |args, _ctx| async move {
            let rotation = args["rotation"].as_str().unwrap_or("unknown");
            Ok(json!({ "paged": true, "rotation": rotation, "ack_deadline_seconds": 300 }))
        },
    )
}

/// The host's own guardrail: paging is a real-world side effect, so the
/// severity gate lives outside the prompt where the model cannot talk its
/// way past it.
fn paging_policy() -> ToolPolicy {
    ToolPolicy::new().rule(|call: &ToolCall| {
        if call.name != "page_oncall" {
            return PolicyOutcome::Allow;
        }
        match call
            .arguments
            .get("severity")
            .and_then(|value| value.as_str())
        {
            Some("sev1") => PolicyOutcome::Allow,
            other => PolicyOutcome::Deny(format!(
                "paging is restricted to sev1 incidents (requested: {})",
                other.unwrap_or("unspecified")
            )),
        }
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempWorkspace::new("triage-agent");
    let harness = Harness::builder().workspace(workspace.path()).build()?;

    // The agent definition: what it is, what it can do, what it may not do.
    let agent = harness
        .agent(triage_script())
        .name("triage")
        .system_prompt(TRIAGE_PROMPT)
        .tool(classify_severity())
        .tool(page_oncall())
        .policy(paging_policy())
        .events(true)
        .build()?;

    let alert = "PagerDuty: checkout service returning 503 for 40% of requests";
    let request = RunRequest::new(format!("Triage this alert: {alert}")).on_event(|event| {
        if let HarnessEvent::ToolCall {
            tool_name, input, ..
        } = event
        {
            println!("  [tool] {tool_name} {input}");
        }
    });

    let result = agent.run(request).await?;
    println!("\n{}", result.text);
    assert!(result.text.contains("sev1"), "expected a sev1 triage");

    // The same definition, with a model that over-escalates: the host
    // policy refuses the page and the agent reports that instead.
    let overeager = harness
        .agent(overpaging_script())
        .name("triage")
        .system_prompt(TRIAGE_PROMPT)
        .tool(classify_severity())
        .tool(page_oncall())
        .policy(paging_policy())
        .build()?;

    let result = overeager
        .run(RunRequest::new(
            "Triage this alert: nightly backup job finished 4 minutes late",
        ))
        .await?;
    println!("\n{}", result.text);
    assert!(
        result.text.contains("no page"),
        "expected the page to be refused"
    );

    Ok(())
}

/// The happy path: classify, page, report.
fn triage_script() -> ScriptedModel {
    ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("Classifying the alert.".into()),
            calls: vec![call(
                "t1",
                "classify_severity",
                json!({ "alert": "checkout service returning 503 for 40% of requests" }),
            )],
            usage: None,
        },
        ModelResponse::ToolCalls {
            content: Some("Sev1 confirmed, paging the rotation.".into()),
            calls: vec![call(
                "t2",
                "page_oncall",
                json!({
                    "rotation": "checkout-primary",
                    "severity": "sev1",
                    "summary": "checkout 503s at 40%"
                }),
            )],
            usage: None,
        },
        ModelResponse::final_text(
            "sev1: customer-facing revenue path is unavailable. Paged checkout-primary; \
             ack expected within 5 minutes.",
        ),
    ])
}

/// The guarded path: the model over-escalates, the policy denies the page,
/// and the final answer reports the refusal instead of a page.
fn overpaging_script() -> ScriptedModel {
    ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("Classifying the alert.".into()),
            calls: vec![call(
                "t1",
                "classify_severity",
                json!({ "alert": "nightly backup job finished 4 minutes late" }),
            )],
            usage: None,
        },
        ModelResponse::ToolCalls {
            content: Some("Paging anyway.".into()),
            calls: vec![call(
                "t2",
                "page_oncall",
                json!({
                    "rotation": "platform-primary",
                    "severity": "sev3",
                    "summary": "backup job late"
                }),
            )],
            usage: None,
        },
        ModelResponse::final_text(
            "sev3: no customer impact observed. Sent no page - the host policy restricts \
             paging to sev1, so this stays on the morning review queue.",
        ),
    ])
}
