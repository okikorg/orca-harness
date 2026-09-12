//! Customer support agent: a multi-turn conversation with a refund limit.
//!
//! The agent looks an order up, answers the customer, and on the follow-up
//! turn issues a refund. Two things make it a support agent rather than a
//! chatbot: durable memory, so what the customer told it in an earlier
//! conversation is recalled automatically, and a refund ceiling enforced
//! by a host policy rather than by the prompt.
//!
//! The agent definition is the first thing in `main`: system prompt,
//! account tools, refund policy, and memory. Everything below `main` is
//! scaffolding.
//!
//! Deterministic: the model is scripted and never calls a provider.

#[path = "../support/mod.rs"]
mod support;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{FnTool, ModelResponse, ToolCall};
use orca_harness_extensions::{PolicyOutcome, ToolPolicy};
use orca_harness_sdk::{Harness, MemoryConfig, RunRequest};
use serde_json::json;
use support::TempWorkspace;

/// Refunds above this figure go to a human. The prompt states the rule so
/// the model cooperates; the policy enforces it so cooperation is optional.
const REFUND_LIMIT_USD: f64 = 200.0;

const SUPPORT_PROMPT: &str = "\
You are a customer support agent for an online store.
- Look the order up with `lookup_order` before making any claim about it.
- You may refund up to $200 with `issue_refund`; anything larger must be
  handed to a human supervisor.
- Honor what memory recalls about this customer's stated preferences.
- Be brief and concrete: state what you found and what you did.";

/// Reads an order. A real deployment would query the order service.
fn lookup_order() -> FnTool {
    FnTool::new(
        "lookup_order",
        "Look up an order by its id and return status, total, and items.",
        json!({
            "type": "object",
            "properties": { "order_id": { "type": "string" } },
            "required": ["order_id"]
        }),
        |args, _ctx| async move {
            let order_id = args["order_id"].as_str().unwrap_or_default();
            Ok(json!({
                "order_id": order_id,
                "status": "delivered",
                "delivered_on": "2026-09-04",
                "total_usd": 128.40,
                "items": ["Wool runner - size 10", "Spare laces"],
                "tracking_note": "left at back door"
            }))
        },
    )
}

/// Moves money. The policy below decides whether the call reaches it.
fn issue_refund() -> FnTool {
    FnTool::new(
        "issue_refund",
        "Refund an order, up to $200. Larger amounts require a supervisor.",
        json!({
            "type": "object",
            "properties": {
                "order_id": { "type": "string" },
                "amount_usd": { "type": "number" },
                "reason": { "type": "string" }
            },
            "required": ["order_id", "amount_usd", "reason"]
        }),
        |args, _ctx| async move {
            let amount = args["amount_usd"].as_f64().unwrap_or_default();
            Ok(json!({
                "refunded_usd": amount,
                "settles_in_days": 3,
                "confirmation": "RF-40912"
            }))
        },
    )
}

/// Spending authority belongs to the host, not to the transcript.
fn refund_policy() -> ToolPolicy {
    ToolPolicy::new().rule(|call: &ToolCall| {
        if call.name != "issue_refund" {
            return PolicyOutcome::Allow;
        }
        match call.arguments.get("amount_usd").and_then(|v| v.as_f64()) {
            Some(amount) if amount <= REFUND_LIMIT_USD => PolicyOutcome::Allow,
            Some(amount) => PolicyOutcome::Deny(format!(
                "refunds over ${REFUND_LIMIT_USD:.0} need a supervisor (requested: ${amount:.2})"
            )),
            None => PolicyOutcome::Deny("refund amount missing".into()),
        }
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempWorkspace::new("support-agent");
    let harness = Harness::builder().workspace(workspace.path()).build()?;

    // What an earlier conversation learned about this customer. Automatic
    // recall puts it in front of the model without the host restating it.
    let memory = harness.memory()?;
    memory.save(
        "Customer prefers refunds to the original card, never store credit.",
        "preference",
        false,
        "ticket-8812",
    )?;

    // The agent definition: what it is, what it can do, what it may not do.
    let agent = harness
        .agent(support_script())
        .name("support")
        .system_prompt(SUPPORT_PROMPT)
        .tool(lookup_order())
        .tool(issue_refund())
        .policy(refund_policy())
        .memory(MemoryConfig::new(memory).automatic_recall(true))
        .build()?;

    // A support conversation is multi-turn, so it needs a session: the
    // follow-up has to see the order the first turn looked up.
    let session = agent.new_session().open()?;

    let first = session
        .run(RunRequest::new(
            "Order A-2291 arrived with one shoe scuffed. What can you see?",
        ))
        .await?;
    println!("turn 1:\n{}\n", first.text);

    let second = session
        .run(RunRequest::new(
            "Please refund me for the scuffed shoe, half the order value is fine.",
        ))
        .await?;
    println!("turn 2:\n{}\n", second.text);
    assert!(
        second.text.contains("RF-40912"),
        "expected the refund to go through under the limit"
    );

    // The same definition against a model that asks for too much: the
    // policy refuses and the agent escalates instead.
    let escalating = harness
        .agent(oversized_refund_script())
        .name("support")
        .system_prompt(SUPPORT_PROMPT)
        .tool(lookup_order())
        .tool(issue_refund())
        .policy(refund_policy())
        .build()?;

    let result = escalating
        .run(RunRequest::new(
            "Refund order A-7740 in full, it was $940 of damaged goods.",
        ))
        .await?;
    println!("escalation:\n{}", result.text);
    assert!(
        result.text.contains("supervisor"),
        "expected an escalation rather than a refund"
    );

    Ok(())
}

/// Turn one looks the order up and answers; turn two refunds within the
/// limit. One script serves both turns of the session in order.
fn support_script() -> ScriptedModel {
    ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("Pulling the order up.".into()),
            calls: vec![call("s1", "lookup_order", json!({ "order_id": "A-2291" }))],
            usage: None,
        },
        ModelResponse::final_text(
            "Order A-2291 (Wool runner size 10, spare laces, $128.40) was delivered \
             on 2026-09-04 and left at the back door. I can refund part of it for the \
             scuffing - say the word.",
        ),
        ModelResponse::ToolCalls {
            content: Some("Refunding half the order to the original card.".into()),
            calls: vec![call(
                "s2",
                "issue_refund",
                json!({
                    "order_id": "A-2291",
                    "amount_usd": 64.20,
                    "reason": "scuffed item, partial refund"
                }),
            )],
            usage: None,
        },
        ModelResponse::final_text(
            "Refunded $64.20 to your original card as you prefer - confirmation RF-40912, \
             settling in about 3 days.",
        ),
    ])
}

/// The guarded path: the requested refund exceeds the host's limit, the
/// policy denies it, and the agent hands the ticket to a human.
fn oversized_refund_script() -> ScriptedModel {
    ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("Refunding in full.".into()),
            calls: vec![call(
                "s1",
                "issue_refund",
                json!({
                    "order_id": "A-7740",
                    "amount_usd": 940.0,
                    "reason": "damaged goods, full refund"
                }),
            )],
            usage: None,
        },
        ModelResponse::final_text(
            "A $940 refund is over the $200 I can authorize, so I did not issue it. \
             I have flagged order A-7740 for a supervisor, who will call you back today.",
        ),
    ])
}
