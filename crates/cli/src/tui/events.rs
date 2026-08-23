//! The terminal event loop and the harness-event plumbing: raw
//! crossterm terminal events (`handle_terminal_event`), the worker's
//! `UiMsg` stream (`handle_ui_msg`), and the transcription of harness
//! run events into the on-screen activity rail and transcript
//! (`handle_harness_event` / `handle_subagent_event`).

use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

use crossterm::event::{Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::text::{Line, Span};
use tokio::sync::mpsc;

use orca_harness_extensions::HarnessEvent;

use crate::clipboard;
use crate::components::picker::ListPicker;
use crate::components::transcript::BlockSpacing;
use crate::msg::{UiMsg, WorkerCmd};
use crate::view::{self, theme};

use super::format::{byte_index, elapsed_label};
use super::input::{insert_paste, marker_ending_at, marker_starting_at, remove_marker};
use super::render::{
    mention_starts_at, remove_location_mention_before_cursor, replay_transcript,
    reset_conversation_ui, transcript_content_width, workspace_locations,
};
use super::state::{
    App, LocationPicker, ModelPicker, Overlay, RunState, SpawnActivity, ToolActivity, ToolRecord,
    ViewMode,
};
use super::PALETTE_ROWS;
use super::{
    clear_tool_connectors, copy_command, expand_latest_work, expand_tool, handle_approval_key,
    handle_overlay_key, history_nav, palette_move, palette_selection, push_notice,
    push_wrapped_lines, scroll_transcript, start_next_queued_prompt, submit, SCROLL_PAGE,
};

pub(crate) fn flush_terminal_requests(app: &mut App) -> io::Result<()> {
    let mut out = io::stdout();
    if let Some(text) = app.clipboard_pending.take() {
        out.write_all(clipboard::osc52(&text).as_bytes())?;
        out.flush()?;
    }
    Ok(())
}
pub(crate) fn handle_terminal_event(
    app: &mut App,
    event: CtEvent,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    width: usize,
) {
    let key = match event {
        CtEvent::Key(key) => key,
        CtEvent::Mouse(mouse) => {
            let split_boundary = ((width as u32 * 58) / 100) as u16;
            let over_inspector =
                app.view_mode == ViewMode::Split && width >= 100 && mouse.column >= split_boundary;
            match mouse.kind {
                MouseEventKind::ScrollUp if over_inspector => {
                    app.split_scroll = app.split_scroll.saturating_sub(3)
                }
                MouseEventKind::ScrollDown if over_inspector => {
                    app.split_scroll = app.split_scroll.saturating_add(3)
                }
                // An open palette is a list, not part of the transcript:
                // the wheel moves its selection rather than scrolling
                // the conversation out from under it.
                MouseEventKind::ScrollUp if app.palette_query().is_some() => palette_move(app, -1),
                MouseEventKind::ScrollDown if app.palette_query().is_some() => palette_move(app, 1),
                MouseEventKind::ScrollUp => scroll_transcript(app, 3),
                MouseEventKind::ScrollDown => scroll_transcript(app, -3),
                _ => {}
            }
            return;
        }
        // A paste is composer input only: an open approval or overlay is
        // a keystroke menu with nowhere to put the text.
        CtEvent::Paste(text) => {
            if app.approval.is_none() && app.overlay.is_none() {
                insert_paste(app, &text);
            }
            return;
        }
        _ => return,
    };
    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
        return;
    }
    if app.approval.is_some() {
        handle_approval_key(app, key);
        return;
    }
    if app.overlay.is_some() {
        handle_overlay_key(app, key, worker);
        return;
    }
    let content_width = transcript_content_width(app, width);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char('c') if ctrl => {
            if let RunState::Running { cancel, .. } = &app.run {
                cancel.cancel();
            } else if !app.composer.is_empty() {
                app.composer.clear();
                app.cursor = 0;
            } else {
                app.quit = true;
            }
        }
        KeyCode::Char('d') if ctrl && app.composer.is_empty() => app.quit = true,
        KeyCode::Char('a') if ctrl => app.cursor = 0,
        KeyCode::Char('e') if ctrl => app.cursor = app.composer.chars().count(),
        KeyCode::Char('u') if ctrl => {
            app.composer.clear();
            app.cursor = 0;
        }
        KeyCode::Char('o') if ctrl => {
            if !expand_latest_work(app) {
                expand_tool(app, 1, content_width);
            }
        }
        // Deliberately not ctrl+s (XOFF on most terminals — the app would
        // appear to hang) and not a plain letter the composer needs.
        // ctrl+y copies whichever pane has focus, so the same key means
        // "take what I am looking at" in either half of a split.
        KeyCode::Char('y') if ctrl => {
            copy_command(app, if app.split_focused { "tool" } else { "" })
        }
        // Line scroll on shift+arrows: PgUp/PgDn are fn+arrows on a laptop
        // keyboard, which terminals often swallow for their own scrollback
        // before the app ever sees them.
        KeyCode::Up if shift => scroll_transcript(app, 1),
        KeyCode::Down if shift => scroll_transcript(app, -1),
        KeyCode::Tab
            if app.view_mode == ViewMode::Split
                && width >= 100
                && !app.activity_tools.is_empty() =>
        {
            app.split_focused = !app.split_focused;
            if app.split_tool.is_none() {
                app.split_tool = Some(app.activity_tools.len() - 1);
            }
        }
        KeyCode::Up if app.split_focused => {
            let selected = app
                .split_tool
                .unwrap_or_else(|| app.activity_tools.len().saturating_sub(1));
            app.split_tool = Some(selected.saturating_sub(1));
            app.split_scroll = 0;
        }
        KeyCode::Down if app.split_focused => {
            let last = app.activity_tools.len().saturating_sub(1);
            app.split_tool = Some(app.split_tool.unwrap_or(last).saturating_add(1).min(last));
            app.split_scroll = 0;
        }
        KeyCode::PageUp if app.split_focused => {
            app.split_scroll = app.split_scroll.saturating_sub(SCROLL_PAGE as u16);
        }
        KeyCode::PageDown if app.split_focused => {
            app.split_scroll = app.split_scroll.saturating_add(SCROLL_PAGE as u16);
        }
        // Page keys belong to the palette while it is open, for the same
        // reason the wheel does: the list is what the user is looking at.
        KeyCode::PageUp if app.palette_query().is_some() => {
            palette_move(app, -(PALETTE_ROWS as isize))
        }
        KeyCode::PageDown if app.palette_query().is_some() => {
            palette_move(app, PALETTE_ROWS as isize)
        }
        KeyCode::PageUp => scroll_transcript(app, SCROLL_PAGE as isize),
        KeyCode::PageDown => scroll_transcript(app, -(SCROLL_PAGE as isize)),
        KeyCode::Esc => {
            if app.split_focused {
                app.split_focused = false;
            } else if app.palette_query().is_some() {
                app.composer.clear();
                app.cursor = 0;
                app.palette_index = 0;
            } else if let RunState::Running { cancel, .. } = &app.run {
                cancel.cancel();
            } else {
                app.composer.clear();
                app.cursor = 0;
            }
        }
        KeyCode::Tab => {
            if let Some(spec) = palette_selection(app) {
                app.composer = if spec.takes_args {
                    format!("/{} ", spec.name)
                } else {
                    format!("/{}", spec.name)
                };
                app.cursor = app.composer.chars().count();
            }
        }
        KeyCode::Enter => {
            // Palette open with a selection and no arguments typed:
            // enter uses the highlighted command as-is.
            if let Some(spec) = palette_selection(app) {
                if !app.composer.trim().contains(char::is_whitespace) {
                    app.composer = format!("/{}", spec.name);
                    app.cursor = app.composer.chars().count();
                }
            }
            app.scroll = 0;
            submit(app, worker, content_width);
            app.palette_index = 0;
        }
        KeyCode::Char(c) => {
            let at = byte_index(&app.composer, app.cursor);
            app.composer.insert(at, c);
            app.cursor += 1;
            app.palette_index = 0;
            if c == '@' && mention_starts_at(&app.composer, app.cursor - 1) {
                let entries = workspace_locations(Path::new(&app.cfg.workspace_root));
                app.overlay = Some(Overlay::Locations(LocationPicker {
                    picker: ListPicker::new(entries.len()),
                    entries,
                    query: String::new(),
                    token_start: app.cursor - 1,
                }));
            }
        }
        KeyCode::Backspace => {
            if let Some((start, end)) = marker_ending_at(&app.pastes, &app.composer, app.cursor) {
                remove_marker(app, start, end);
                app.palette_index = 0;
            } else if remove_location_mention_before_cursor(&mut app.composer, &mut app.cursor) {
                app.palette_index = 0;
            } else if app.cursor > 0 {
                let at = byte_index(&app.composer, app.cursor - 1);
                app.composer.remove(at);
                app.cursor -= 1;
                app.palette_index = 0;
            }
        }
        KeyCode::Delete => {
            if let Some((start, end)) = marker_starting_at(&app.pastes, &app.composer, app.cursor) {
                remove_marker(app, start, end);
                app.palette_index = 0;
            } else if app.cursor < app.composer.chars().count() {
                let at = byte_index(&app.composer, app.cursor);
                app.composer.remove(at);
                app.palette_index = 0;
            }
        }
        // Arrows step over a marker whole too: landing inside one would
        // let the next keystroke corrupt it into text that no longer
        // expands.
        KeyCode::Left => {
            app.cursor = match marker_ending_at(&app.pastes, &app.composer, app.cursor) {
                Some((start, _)) => start,
                None => app.cursor.saturating_sub(1),
            }
        }
        KeyCode::Right => {
            app.cursor = match marker_starting_at(&app.pastes, &app.composer, app.cursor) {
                Some((_, end)) => end,
                None => (app.cursor + 1).min(app.composer.chars().count()),
            }
        }
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.composer.chars().count(),
        KeyCode::Up => {
            if app.palette_query().is_some() {
                palette_move(app, -1);
            } else {
                history_nav(app, -1);
            }
        }
        KeyCode::Down => {
            if app.palette_query().is_some() {
                palette_move(app, 1);
            } else {
                history_nav(app, 1);
            }
        }
        _ => {}
    }
}

pub(crate) fn handle_ui_msg(
    app: &mut App,
    msg: UiMsg,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    width: usize,
) {
    match msg {
        UiMsg::Event(event) => handle_harness_event(app, event, width),
        UiMsg::SubagentEvent {
            id,
            parent_id,
            depth,
            call_id,
            event,
        } => handle_subagent_event(app, id, parent_id, depth, call_id, event),
        UiMsg::Approval(request) => app.approval = Some(request),
        UiMsg::Models(result) => {
            let t = theme();
            let seed = app.picker_pending.take().unwrap_or_default();
            // Learn the active model's window from the catalog in passing.
            if let Ok(models) = &result {
                if let Some(info) = models.iter().find(|m| m.id == app.cfg.model_name) {
                    if info.context_length.is_some() {
                        app.context_window = info.context_length;
                    }
                }
            }
            match result {
                Ok(models) if models.is_empty() => {
                    app.push_line(Line::from(Span::styled("no models available", t.dim)));
                }
                Ok(models) => {
                    app.overlay = Some(Overlay::Models(ModelPicker {
                        models,
                        filter: seed,
                        index: 0,
                    }));
                }
                Err(err) => app.push_line(Line::from(Span::styled(
                    format!("model list failed: {err}"),
                    t.error,
                ))),
            }
        }
        UiMsg::ModelChanged(id) => {
            app.cfg.model_name = id.clone();
            app.push_line(Line::from(Span::styled(
                format!("model: {id}"),
                theme().dim,
            )));
        }
        UiMsg::ContextWindow(window) => {
            // Best-effort discovery: never wipe a window the model picker
            // already stashed with a probe that found nothing.
            if window.is_some() {
                app.context_window = window;
            }
        }
        UiMsg::Compacted(result) => match result {
            Ok(report) => {
                // Until the next model step reports real usage, the
                // report's estimate is the best context figure we have.
                app.context_tokens = report.est_tokens_after as u64;
                let pct =
                    100.0 * report.est_tokens_after as f64 / report.est_tokens_before.max(1) as f64;
                app.push_line(Line::from(Span::styled(
                    format!(
                        "compacted: {} -> {} messages · ~{} -> ~{} est tokens ({pct:.1}%)",
                        report.messages_before,
                        report.messages_after,
                        report.est_tokens_before,
                        report.est_tokens_after,
                    ),
                    theme().dim,
                )));
                if report.elided_results > 0 {
                    app.push_line(Line::from(Span::styled(
                        format!(
                            "{} tool outputs ({} KB) elided to store, recoverable via read_tool_result",
                            report.elided_results,
                            report.elided_bytes / 1024,
                        ),
                        theme().dim,
                    )));
                }
            }
            Err(err) => {
                app.push_line(Line::from(Span::styled(
                    format!("compact: {err}"),
                    theme().dim,
                )));
            }
        },
        UiMsg::Notice(text) => {
            push_notice(app, text);
        }
        UiMsg::SessionCleared { id } => {
            app.cfg.session_id = Some(id.clone());
            push_notice(
                app,
                format!("session {id} cleared · background work stopped"),
            );
        }
        UiMsg::ContextRewound { messages, notice } => {
            // The transcript is redrawn from the shortened context, but
            // the token totals are not conversation state — they record
            // what this session actually spent, and rewinding does not
            // un-spend it. Occupancy is left to the next model step.
            let spent = (
                app.tokens_in,
                app.tokens_out,
                app.cache_read_total,
                app.cache_write_total,
                app.usage_steps,
            );
            reset_conversation_ui(app);
            (
                app.tokens_in,
                app.tokens_out,
                app.cache_read_total,
                app.cache_write_total,
                app.usage_steps,
            ) = spent;
            push_notice(app, notice);
            replay_transcript(app, &messages, width);
        }
        UiMsg::SessionForked { id, parent } => {
            app.cfg.session_id = Some(id.clone());
            push_notice(
                app,
                format!("forked to session {id} · {parent} is left as it was"),
            );
        }
        UiMsg::SessionLoaded { id, messages } => {
            reset_conversation_ui(app);
            app.cfg.session_id = Some(id.clone());
            push_notice(
                app,
                format!("resumed session {id} ({} messages)", messages.len()),
            );
            replay_transcript(app, &messages, width);
        }
        UiMsg::ProviderChanged { provider, model } => {
            app.cfg.provider = provider;
            app.cfg.model_name = model.clone();
            app.context_window = None;
            app.push_line(Line::from(Span::styled(
                format!("provider: {} · model: {model}", provider.label()),
                theme().dim,
            )));
        }
        UiMsg::RunDone(result) => {
            let completed = result.is_ok();
            let turn_elapsed = match &app.run {
                RunState::Running { started, .. } => Some(started.elapsed()),
                RunState::Idle => None,
            };
            // Flush any partial stream (interrupted mid-generation).
            if result.is_err() {
                app.commit_activity(width);
                let partial = if !app.text.trim().is_empty() {
                    Some(std::mem::take(&mut app.text))
                } else {
                    app.pending_assistant.take()
                };
                if let Some(partial) = partial {
                    app.push_markdown_block(&partial, width, BlockSpacing::Section);
                    // Interrupted output is often exactly what the user
                    // wanted to keep — that is why they interrupted.
                    app.last_answer = Some(partial);
                }
            }
            app.text.clear();
            app.run = RunState::Idle;
            app.approval = None;
            if let Err(err) = result {
                let (style, label) = if err.to_lowercase().contains("cancel") {
                    (theme().dim, "interrupted".to_string())
                } else {
                    (theme().error, format!("run failed: {err}"))
                };
                let mut lines = Vec::new();
                push_wrapped_lines(&mut lines, &label, "  ", style, width);
                app.push_transcript_block(lines, BlockSpacing::Tight);
            }
            if completed {
                if let Some(elapsed) = turn_elapsed {
                    let calls = app.turn_tool_calls;
                    let plural = if calls == 1 { "" } else { "s" };
                    app.last_turn_summary = Some(format!(
                        "Turn took {:.1}s and took {calls} tool call{plural}",
                        elapsed.as_secs_f64(),
                    ));
                }
                start_next_queued_prompt(app, worker, width);
            }
        }
        UiMsg::ShellDone => {
            app.commit_activity(width);
            let elapsed = match &app.run {
                RunState::Running { started, .. } => Some(started.elapsed()),
                RunState::Idle => None,
            };
            app.run = RunState::Idle;
            app.approval = None;
            if let Some(elapsed) = elapsed {
                app.last_turn_summary =
                    Some(format!("Shell command took {:.1}s", elapsed.as_secs_f64()));
            }
            start_next_queued_prompt(app, worker, width);
        }
    }
}

pub(crate) fn handle_harness_event(app: &mut App, event: HarnessEvent, width: usize) {
    match event {
        HarnessEvent::AssistantDelta { text } => {
            if app.text.is_empty() && !text.is_empty() {
                app.commit_settled_tools(width);
            }
            app.text.push_str(&text);
        }
        HarnessEvent::ReasoningDelta { text } => {
            if app.reasoning.is_empty() && !text.is_empty() {
                app.commit_settled_tools(width);
                app.reasoning_started = Some(Instant::now());
            }
            app.reasoning.push_str(&text);
        }
        HarnessEvent::Assistant { message } => {
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
                elapsed: None,
                output: None,
                is_error: false,
                approval: None,
            });
            if !app.split_focused {
                app.split_tool = Some(index);
                app.split_scroll = 0;
            }
            app.pending_calls.insert(tool_call_id, index);
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
                    activity.elapsed = Some(activity.started.elapsed());
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
            app.cache_read_total += usage.cache_read_tokens;
            app.cache_write_total += usage.cache_create_tokens;
            app.usage_steps += 1;
            // The latest step's full footprint (uncached + cached input +
            // output) is what the next request will carry. Authoritative:
            // replaces any bytes/4 estimates accumulated since last step.
            app.context_tokens = usage.context_tokens();
        }
        HarnessEvent::Result { message } => {
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
        .filter(|(_, spawn)| spawn.call_id == call_id)
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
                elapsed: None,
                output: None,
                is_error: false,
                approval: None,
            });
            spawn.pending.insert(tool_call_id, index);
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
                        tool.elapsed = Some(tool.started.elapsed());
                        tool.output = Some(output);
                        tool.is_error = is_error;
                    }
                }
            }
        }
        _ => {}
    }
}
