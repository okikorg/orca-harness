//! Imported conversation history: the shape rules a transcript must meet
//! before the kernel can resume from it, and how the agent's system prompt
//! combines with an imported one.

use std::collections::BTreeSet;

use orca_harness_core::{Context, Message};

use crate::{Agent, SdkError};

/// Builds the session context for an imported history. The agent's system
/// prompt is authoritative: it replaces a leading imported `System` message
/// or is prepended when the import has none. Without an agent prompt an
/// imported leading `System` message is kept as-is.
pub(super) fn imported_context(
    agent: &Agent,
    mut messages: Vec<Message>,
) -> Result<Context, SdkError> {
    validate(&messages)?;
    if let Some(prompt) = &agent.inner.system_prompt {
        let prompt = Message::System {
            content: prompt.clone(),
        };
        match messages.first_mut() {
            Some(first @ Message::System { .. }) => *first = prompt,
            _ => messages.insert(0, prompt),
        }
    }
    let mut context = Context::new();
    for message in messages {
        context.push(message);
    }
    Ok(context)
}

/// Rejects transcripts the kernel cannot resume from: a `System` message
/// anywhere but index 0 (or more than one), a `Tool` message that does not
/// answer the tool calls of the assistant turn just before it, and an
/// assistant turn with tool calls that the next message does not answer.
fn validate(messages: &[Message]) -> Result<(), SdkError> {
    for (index, message) in messages.iter().enumerate() {
        match message {
            Message::System { .. } if index != 0 => {
                return Err(invalid(format!(
                    "system message at index {index}; only one is allowed and it must come first"
                )));
            }
            Message::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                let expected: BTreeSet<&str> =
                    tool_calls.iter().map(|call| call.id.as_str()).collect();
                let answered = match messages.get(index + 1) {
                    Some(Message::Tool { results }) => results
                        .iter()
                        .map(|result| result.call_id.as_str())
                        .collect(),
                    _ => BTreeSet::new(),
                };
                if answered != expected {
                    return Err(invalid(format!(
                        "assistant message at index {index} calls tools {expected:?} but the next message answers {answered:?}"
                    )));
                }
            }
            Message::Tool { .. } => {
                let answers_calls = index > 0
                    && matches!(
                        &messages[index - 1],
                        Message::Assistant { tool_calls, .. } if !tool_calls.is_empty()
                    );
                if !answers_calls {
                    return Err(invalid(format!(
                        "tool message at index {index} does not follow an assistant message with tool calls"
                    )));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn invalid(message: String) -> SdkError {
    SdkError::InvalidContext(message)
}
