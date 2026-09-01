use super::super::*;
use crate::tui::components::transcript::BlockSpacing;
use crate::tui::state::{SpawnActivity, SubagentDisplay, ToolRecord};
use orca_harness_extensions::HarnessEvent;

pub(crate) fn handle_harness_event(app: &mut App, event: HarnessEvent, width: usize) {
    match event {
        HarnessEvent::AssistantDelta { text } => {
            app.consume_turn_text(&text);
            if app.text.is_empty() && !text.is_empty() {
                app.commit_settled_tools(width);
            }
            app.text.push_str(&text);
        }
        HarnessEvent::ReasoningDelta { text } => {
            app.consume_turn_reasoning(&text);
            if app.reasoning.is_empty() && !text.is_empty() {
                app.commit_settled_tools(width);
                app.reasoning_started = Some(Instant::now());
            }
            app.reasoning.push_str(&text);
        }
        HarnessEvent::ToolInputDelta { text } => {
            app.consume_turn_tool_input(&text);
        }
        HarnessEvent::Assistant { message } => {
            // Non-streaming adapters produce no deltas, so use the complete
            // message as this step's estimate only when none was seen.
            app.consume_turn_text_if_unseen(&message);
            // `Assistant` closes the current model phase. Commit its
            // reasoning and any preceding tool batch before retaining the
            // message that follows it in the event stream.
            app.commit_activity(width);
            app.text.clear();
            app.pending_assistant = Some(message);
        }
        HarnessEvent::ToolCall {
            tool_call_id,
            tool_name,
            input,
        } => {
            app.settle_turn_output_estimate();
            clear_tool_connectors(&mut app.transcript);
            clear_tool_connectors(&mut app.pending_history);
            app.flush_reasoning();
            if let Some(message) = app.pending_assistant.take() {
                if !message.trim().is_empty() {
                    app.push_markdown_block(&message, width, BlockSpacing::Tight);
                    // Prose said on the way to a tool call is still the
                    // most recent thing the model wrote.
                    app.last_answer = Some(message);
                }
            }
            let call_line = view::tool_call_line(&tool_name, &input);
            app.turn_tool_calls += 1;
            let index = app.activity_tools.len();
            app.activity_tools.push(ToolActivity {
                call_id: tool_call_id.clone(),
                call_line,
                tool_name,
                input,
                started: Instant::now(),
                execution_started: None,
                execution_elapsed: None,
                elapsed: None,
                output: None,
                is_error: false,
                approval: None,
            });
            app.split_tool = Some(index);
            app.split_scroll = 0;
            app.pending_calls.insert(tool_call_id, index);
        }
        HarnessEvent::ToolStarted { tool_call_id, .. } => {
            if let Some(index) = app.pending_calls.get(&tool_call_id).copied() {
                if let Some(activity) = app.activity_tools.get_mut(index) {
                    activity.execution_started.get_or_insert_with(Instant::now);
                }
            }
        }
        HarnessEvent::ToolFinished {
            tool_call_id,
            is_error,
            ..
        } => {
            if let Some(index) = app.pending_calls.get(&tool_call_id).copied() {
                if let Some(activity) = app.activity_tools.get_mut(index) {
                    let now = Instant::now();
                    activity.elapsed = Some(now.duration_since(activity.started));
                    activity.execution_elapsed = activity
                        .execution_started
                        .map(|started| now.duration_since(started));
                    activity.is_error = is_error;
                }
            }
        }
        HarnessEvent::ToolResult {
            tool_call_id,
            tool_name,
            output,
            is_error,
        } => {
            // Trailing estimate, pi-style: the result joins the context
            // now but is only billed at the next model step, which then
            // overwrites this with the provider's count.
            let result_bytes = serde_json::to_string(&output).map(|s| s.len()).unwrap_or(0);
            app.context_tokens += (result_bytes / 4) as u64;
            let index = app.pending_calls.remove(&tool_call_id);
            let call_line = index
                .and_then(|index| app.activity_tools.get_mut(index))
                .map(|activity| {
                    let now = Instant::now();
                    activity
                        .elapsed
                        .get_or_insert_with(|| now.duration_since(activity.started));
                    if activity.execution_elapsed.is_none() {
                        activity.execution_elapsed = activity
                            .execution_started
                            .map(|started| now.duration_since(started));
                    }
                    activity.output = Some(output.clone());
                    activity.is_error = is_error;
                    activity.call_line.clone()
                })
                .unwrap_or_else(|| tool_name.clone());
            let inner = if tool_name == "subagent" {
                fold_subagent_activity(app, &tool_call_id)
            } else {
                Vec::new()
            };
            app.push_record(ToolRecord {
                call_line,
                tool_name,
                output,
                inner,
            });
        }
        HarnessEvent::Usage { usage } => {
            app.tokens_in += usage.input_tokens;
            app.tokens_out += usage.output_tokens;
            app.reconcile_turn_output(usage.output_tokens);
            app.cache_read_total += usage.cache_read_tokens;
            app.cache_write_total += usage.cache_create_tokens;
            app.usage_steps += 1;
            // The latest step's full footprint (uncached + cached input +
            // output) is what the next request will carry. Authoritative:
            // replaces any bytes/4 estimates accumulated since last step.
            app.context_tokens = usage.context_tokens();
        }
        HarnessEvent::Result { message } => {
            app.settle_turn_output_estimate();
            app.commit_activity(width);
            let answer = if message.is_empty() {
                app.pending_assistant.take().unwrap_or_default()
            } else {
                app.pending_assistant = None;
                message
            };
            app.text.clear();
            if !answer.trim().is_empty() {
                app.push_markdown_block(&answer, width, BlockSpacing::Section);
                app.last_answer = Some(answer);
            }
        }
        HarnessEvent::AgentStart | HarnessEvent::Error { .. } => {}
    }
}

pub(crate) fn fold_subagent_activity(app: &mut App, call_id: &str) -> Vec<String> {
    let mut roots: Vec<u64> = app
        .subagent_activity
        .iter()
        .filter(|(_, spawn)| spawn.parent_id.is_none() && spawn.call_id == call_id)
        .map(|(id, _)| *id)
        .collect();
    roots.sort_unstable();
    let mut lines = Vec::new();
    for id in roots {
        collect_spawn_log(app, id, &mut lines);
    }
    lines
}

pub(crate) fn collect_spawn_log(app: &mut App, id: u64, lines: &mut Vec<String>) {
    let Some(spawn) = app.subagent_activity.remove(&id) else {
        return;
    };
    let indent = "  ".repeat(spawn.depth as usize);
    for tool in &spawn.tools {
        let glyph = match &tool.output {
            Some(_) if tool.is_error => "×",
            Some(_) => "✓",
            None => "□",
        };
        let elapsed = tool.elapsed.unwrap_or_default();
        lines.push(format!(
            "{indent}{glyph} {} · {}",
            tool.call_line,
            elapsed_label(elapsed)
        ));
    }
    let mut children: Vec<u64> = app
        .subagent_activity
        .iter()
        .filter(|(_, s)| s.parent_id == Some(id))
        .map(|(child, _)| *child)
        .collect();
    children.sort_unstable();
    for child in children {
        collect_spawn_log(app, child, lines);
    }
}

pub(crate) fn start_subagent(
    app: &mut App,
    id: u64,
    parent_id: Option<u64>,
    depth: u32,
    call_id: String,
    task: String,
    identity: Option<orca_harness_tools::SubagentIdentity>,
) {
    if parent_id.is_none() {
        if let Some(identity) = identity.clone() {
            app.subagent_display.insert(
                call_id.clone(),
                SubagentDisplay {
                    task: task.clone(),
                    identity,
                },
            );
        }
    }
    app.subagent_activity.insert(
        id,
        SpawnActivity {
            call_id,
            parent_id,
            depth,
            task,
            identity,
            tools: Vec::new(),
            pending: std::collections::HashMap::new(),
        },
    );
}

pub(crate) fn handle_subagent_event(
    app: &mut App,
    id: u64,
    parent_id: Option<u64>,
    depth: u32,
    call_id: String,
    event: HarnessEvent,
) {
    match event {
        HarnessEvent::ToolCall {
            tool_call_id,
            tool_name,
            input,
        } => {
            let spawn = app
                .subagent_activity
                .entry(id)
                .or_insert_with(|| SpawnActivity {
                    call_id,
                    parent_id,
                    depth,
                    task: String::new(),
                    identity: None,
                    tools: Vec::new(),
                    pending: std::collections::HashMap::new(),
                });
            let call_line = view::tool_call_line(&tool_name, &input);
            let index = spawn.tools.len();
            spawn.tools.push(ToolActivity {
                call_id: tool_call_id.clone(),
                call_line,
                tool_name,
                input,
                started: Instant::now(),
                execution_started: None,
                execution_elapsed: None,
                elapsed: None,
                output: None,
                is_error: false,
                approval: None,
            });
            spawn.pending.insert(tool_call_id, index);
        }
        HarnessEvent::ToolStarted { tool_call_id, .. } => {
            if let Some(spawn) = app.subagent_activity.get_mut(&id) {
                if let Some(index) = spawn.pending.get(&tool_call_id).copied() {
                    if let Some(tool) = spawn.tools.get_mut(index) {
                        tool.execution_started.get_or_insert_with(Instant::now);
                    }
                }
            }
        }
        HarnessEvent::ToolFinished {
            tool_call_id,
            is_error,
            ..
        } => {
            if let Some(spawn) = app.subagent_activity.get_mut(&id) {
                if let Some(index) = spawn.pending.get(&tool_call_id).copied() {
                    if let Some(tool) = spawn.tools.get_mut(index) {
                        let now = Instant::now();
                        tool.elapsed = Some(now.duration_since(tool.started));
                        tool.execution_elapsed = tool
                            .execution_started
                            .map(|started| now.duration_since(started));
                        tool.is_error = is_error;
                    }
                }
            }
        }
        HarnessEvent::ToolResult {
            tool_call_id,
            output,
            is_error,
            ..
        } => {
            if let Some(spawn) = app.subagent_activity.get_mut(&id) {
                if let Some(index) = spawn.pending.remove(&tool_call_id) {
                    if let Some(tool) = spawn.tools.get_mut(index) {
                        let now = Instant::now();
                        tool.elapsed
                            .get_or_insert_with(|| now.duration_since(tool.started));
                        if tool.execution_elapsed.is_none() {
                            tool.execution_elapsed = tool
                                .execution_started
                                .map(|started| now.duration_since(started));
                        }
                        tool.output = Some(output);
                        tool.is_error = is_error;
                    }
                }
            }
        }
        _ => {}
    }
}
