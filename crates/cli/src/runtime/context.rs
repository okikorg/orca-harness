use orca_harness_core::{Context, Message, ToolResult};

/// Build a fresh context from a slice of messages. `Context` is
/// append-only by design (it is what the model sees, not a database), so
/// rewinding means rebuilding rather than truncating in place.
pub(crate) fn context_from(messages: &[Message]) -> Context {
    let mut context = Context::new();
    for message in messages {
        context.push(message.clone());
    }
    context
}

/// Where to cut the transcript to drop the last `turns` user turns:
/// returns `(message index to truncate at, turns actually dropped)`, or
/// `None` when there is no user turn to drop.
///
/// The cut always lands *on* a user message. Everything before one is a
/// complete turn — assistant messages with their tool results — so the
/// remaining transcript can never end in tool calls with no results,
/// which a chat-completions endpoint rejects. Asking to rewind further
/// than the conversation goes rewinds all of it rather than failing.
pub(crate) fn rewind_cut(messages: &[Message], turns: usize) -> Option<(usize, usize)> {
    let boundaries: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| matches!(message, Message::User { .. }))
        .map(|(index, _)| index)
        .collect();
    if boundaries.is_empty() {
        return None;
    }
    let keep = boundaries.len().saturating_sub(turns.max(1));
    Some((boundaries[keep], boundaries.len() - keep))
}

/// A cancelled run can leave the transcript ending in assistant tool calls
/// with no results; chat-completions endpoints reject that shape on the
/// next turn, so close them out with synthetic error results.
pub(crate) fn repair_dangling_tool_calls(context: &mut Context) {
    let Some(Message::Assistant { tool_calls, .. }) = context.messages().last() else {
        return;
    };
    if tool_calls.is_empty() {
        return;
    }
    let results = tool_calls
        .iter()
        .map(|call| ToolResult {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            output: serde_json::json!({"error": "cancelled before execution"}),
            is_error: true,
        })
        .collect();
    context.append_tool_results(results);
}
