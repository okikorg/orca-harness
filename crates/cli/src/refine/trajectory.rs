//! Serialize the session into cited events. Provider tool call_ids are
//! not model-readable text on every endpoint, so the host assigns its
//! own stable ids (e01, e02, …) and injects the roster into the
//! proposer prompt; citations are validated against that roster only.

use orca_harness_core::Message;

use super::validate::{DESC_BYTE_LIMIT, PY_ALLOWED, SH_ALLOWED};

/// Per-event ceiling: one enormous tool dump must not crowd out the
/// rest of the session. Head-kept with an explicit marker.
const EVENT_CHAR_LIMIT: usize = 2000;
/// Whole-trajectory ceiling for the proposer prompt; oldest whole
/// events are dropped first so the roster stays honest.
const TRAJECTORY_CHAR_BUDGET: usize = 80_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrajectoryEvent {
    pub id: String,
    pub kind: String,
    pub text: String,
}

/// One event per user message, assistant message, tool call, and tool
/// result. System messages are never part of the evidence. Oversized
/// texts are capped; over the total budget, the oldest whole events are
/// elided (their ids leave the roster with them, so a citation to an
/// elided event correctly fails validation).
pub fn events_from(messages: &[Message]) -> Vec<TrajectoryEvent> {
    let mut events = Vec::new();
    let push = |kind: String, text: String, events: &mut Vec<TrajectoryEvent>| {
        let id = format!("e{:02}", events.len() + 1);
        let text = match text.chars().count() > EVENT_CHAR_LIMIT {
            true => {
                let kept: String = text.chars().take(EVENT_CHAR_LIMIT).collect();
                let dropped = text.chars().count() - EVENT_CHAR_LIMIT;
                format!("{kept}\n[… {dropped} more characters truncated]")
            }
            false => text,
        };
        events.push(TrajectoryEvent { id, kind, text });
    };
    for message in messages {
        match message {
            Message::System { .. } => {}
            Message::User { content, .. } => push("user".into(), content.clone(), &mut events),
            Message::Assistant {
                content,
                tool_calls,
            } => {
                if let Some(text) = content {
                    if !text.is_empty() {
                        push("assistant".into(), text.clone(), &mut events);
                    }
                }
                for call in tool_calls {
                    push(
                        format!("tool:{}", call.name),
                        call.arguments.to_string(),
                        &mut events,
                    );
                }
            }
            Message::Tool { results } => {
                for result in results {
                    let kind = match result.is_error {
                        true => format!("error:{}", result.tool_name),
                        false => format!("result:{}", result.tool_name),
                    };
                    // ToolResult::error wraps the message as {"error": ...};
                    // unwrap plain strings either way so citations read as prose.
                    let text = result
                        .output
                        .as_str()
                        .or_else(|| result.output.get("error").and_then(|e| e.as_str()))
                        .map(String::from)
                        .unwrap_or_else(|| result.output.to_string());
                    push(kind, text, &mut events);
                }
            }
        }
    }
    let mut total: usize = events.iter().map(|e| e.text.chars().count()).sum();
    let mut keep_from = 0;
    while total > TRAJECTORY_CHAR_BUDGET && keep_from < events.len() {
        total -= events[keep_from].text.chars().count();
        keep_from += 1;
    }
    events.split_off(keep_from)
}

/// The full proposer prompt: schema, limits, and allowlist stated up
/// front (weak models cannot honor constraints they were never told),
/// then the existing catalog, the roster, and the events themselves.
pub fn proposer_prompt(events: &[TrajectoryEvent], existing: &[(String, String)]) -> String {
    let roster: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
    let mut out = String::new();
    out.push_str(
        "Review this session trajectory and propose exactly ONE reusable Agent Skill \
         capturing a lesson the session demonstrates. Reply with a single JSON object \
         and nothing else:\n\
         {\"name\": ..., \"description\": ..., \"body\": ..., \
         \"scripts\": [{\"path\": ..., \"code\": ...}], \"citations\": [...]}\n\
         If the trajectory holds no lesson worth a reusable skill, reply \
         {\"none\": true, \"reason\": \"...\"} instead — strongly prefer that over a \
         speculative or one-off skill.\n\
         Hard constraints:\n\
         - exactly one proposal\n\
         - name: kebab-case (lowercase words joined by hyphens)\n",
    );
    if !existing.is_empty() {
        out.push_str("- these skills already exist — do not duplicate their lessons or names:\n");
        for (name, description) in existing.iter().take(40) {
            let short: String = description.chars().take(120).collect();
            out.push_str(&format!("    {name} — {short}\n"));
        }
    }
    out.push_str(&format!(
        "- description: at most {DESC_BYTE_LIMIT} bytes\n\
         - citations: at least one id, each drawn from this roster only \
         (never invent ids, never use provider call_ids): [{}]\n",
        roster.join(", ")
    ));
    out.push_str(&format!(
        "- scripts matter most. Mine the trajectory first for reusable work — command \
         sequences, pipelines, or snippets the session actually ran (especially ones \
         it repeated or fumbled before getting right) — and distill those into \
         scripts/*.sh or scripts/*.py helpers. Let the work pick the language: \
         command sequences and pipelines read best as shell, anything with real \
         parsing or logic reads best as Python — use both in one skill when the work \
         splits that way. Invent a fresh checker only when nothing in the trajectory \
         is worth packaging, and omit scripts only for lessons that are pure \
         judgment.\n\
         - scripts must finish in seconds: prune build and VCS directories (.git, \
         target, node_modules, dist, build, vendor) from every filesystem walk, never \
         read stdin or wait for input, and never loop without a bound.\n\
         - the body must be script-first: tell the agent to RUN the packaged scripts \
         (with their exact paths and arguments) instead of re-deriving the work through \
         individual tool calls — one script run should replace the several calls the \
         trajectory spent. Manual steps belong in the body only where no script fits.\n\
         - Python imports are restricted to this allowlist: [{}]; exec, eval, \
         __import__, input and compile are rejected. Shell is restricted to this \
         command allowlist: [{}].\n\nTrajectory:\n",
        PY_ALLOWED.join(", "),
        SH_ALLOWED.join(", ")
    ));
    for event in events {
        out.push_str(&format!("[{}] {}: {}\n", event.id, event.kind, event.text));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_core::{ToolCall, ToolResult};

    fn sample_messages() -> Vec<Message> {
        let call = ToolCall {
            id: "call_abc".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"command": "cargo test"}),
        };
        vec![
            Message::System {
                content: "system prompt".into(),
            },
            Message::User {
                content: "add retries".into(),
                images: Vec::new(),
            },
            Message::Assistant {
                content: Some("running tests".into()),
                tool_calls: vec![call.clone()],
            },
            Message::Tool {
                results: vec![ToolResult::error(&call, "FAILED: retry_hammers_server")],
            },
        ]
    }

    #[test]
    fn events_get_stable_host_ids_and_skip_system() {
        let events = events_from(&sample_messages());
        let ids: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["e01", "e02", "e03", "e04"]);
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["user", "assistant", "tool:shell", "error:shell"]
        );
        assert!(events.iter().all(|e| !e.text.contains("system prompt")));
    }

    #[test]
    fn oversized_events_are_capped_and_old_events_yield_to_the_budget() {
        // One event over the per-event cap gets a truncation marker.
        let long = "x".repeat(EVENT_CHAR_LIMIT + 500);
        let events = events_from(&[Message::User {
            content: long,
            images: Vec::new(),
        }]);
        assert!(events[0].text.contains("[… 500 more characters truncated]"));
        assert!(events[0].text.chars().count() < EVENT_CHAR_LIMIT + 100);

        // Enough capped events to exceed the whole-trajectory budget:
        // the oldest are elided, ids of survivors are untouched.
        let many: Vec<Message> = (0..60)
            .map(|_| Message::User {
                content: "y".repeat(EVENT_CHAR_LIMIT),
                images: Vec::new(),
            })
            .collect();
        let events = events_from(&many);
        assert!(events.len() < 60, "some events must be elided");
        let total: usize = events.iter().map(|e| e.text.chars().count()).sum();
        assert!(total <= TRAJECTORY_CHAR_BUDGET);
        assert_eq!(events.last().unwrap().id, "e60", "newest events survive");
        assert_ne!(events.first().unwrap().id, "e01", "oldest events go first");
    }

    #[test]
    fn prompt_offers_the_decline_reply_and_lists_existing_skills() {
        let events = events_from(&sample_messages());
        let existing = vec![("repo-orient".to_string(), "map the repository".to_string())];
        let prompt = proposer_prompt(&events, &existing);
        assert!(prompt.contains("{\"none\": true, \"reason\": \"...\"}"));
        assert!(prompt.contains("strongly prefer that over a speculative"));
        assert!(prompt.contains("repo-orient — map the repository"));
        // And an empty catalog adds no section at all.
        assert!(!proposer_prompt(&events, &[]).contains("already exist"));
    }

    #[test]
    fn prompt_states_contract_and_injects_roster_not_call_ids() {
        let events = events_from(&sample_messages());
        let prompt = proposer_prompt(&events, &[]);
        assert!(prompt.contains("exactly one proposal"));
        assert!(prompt.contains("kebab-case"));
        assert!(prompt.contains(&format!("at most {DESC_BYTE_LIMIT} bytes")));
        assert!(prompt.contains("[e01, e02, e03, e04]"));
        assert!(prompt.contains("allowlist"));
        // Scripts are mined from the trajectory first, and the body must
        // route the agent through them rather than loose tool calls.
        assert!(prompt.contains("Mine the trajectory first for reusable work"));
        assert!(prompt.contains("script-first"));
        assert!(prompt.contains("RUN the packaged scripts"));
        assert!(prompt.contains("Let the work pick the language"));
        // Bounded runtime: an unpruned rglob over target/ hung a real run.
        assert!(prompt.contains("must finish in seconds"));
        assert!(prompt.contains("prune build and VCS directories"));
        assert!(prompt.contains("scripts/*.sh or scripts/*.py"));
        // Both language allowlists are spelled out for the proposer.
        assert!(prompt.contains("Python imports are restricted"));
        assert!(prompt.contains("command allowlist"));
        assert!(prompt.contains("xargs"));
        assert!(prompt.contains("[e04] error:shell: FAILED"));
        // The roster is the citation surface; raw provider ids are not.
        assert!(!prompt.contains("call_abc"));
    }
}
