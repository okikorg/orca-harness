use ratatui::text::{Line, Span};

use orca_harness_core::Message;

use crate::tui::command_catalog::filter_commands;
use crate::tui::components::transcript::BlockSpacing;
use crate::view::{self, theme};

use super::super::format::fmt_tokens;
use super::super::{App, ModelPicker, ToolActivity};
use super::pickers::command_picker_row;
/// Exact, source-preserving edit context beneath an `edit_file` row. Small
/// edits remain fully visible; large replacements stay bounded so one call
/// cannot take over the live rail.
pub(crate) fn edit_diff_preview_lines(
    tool: &ToolActivity,
    width: usize,
    continuation: &str,
) -> Vec<Line<'static>> {
    const MAX_EDIT_DIFF_ROWS: usize = 6;

    let old = tool
        .input
        .get("old")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let new = tool
        .input
        .get("new")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let mut changed = old
        .lines()
        .map(|line| ('-', line, theme().dim))
        .chain(new.lines().map(|line| ('+', line, theme().success)))
        .collect::<Vec<_>>();
    if changed.is_empty() && (!old.is_empty() || !new.is_empty()) {
        changed.push((if old.is_empty() { '+' } else { '-' }, "", theme().dim));
    }

    let hidden = changed.len().saturating_sub(MAX_EDIT_DIFF_ROWS);
    let prefix = format!("    {continuation} ");
    let diff_width = width.saturating_sub(prefix.chars().count() + 2).max(16);
    let mut lines = changed
        .into_iter()
        .take(MAX_EDIT_DIFF_ROWS)
        .map(|(marker, source, style)| {
            Line::from(vec![
                Span::styled(prefix.clone(), theme().dim),
                Span::styled(format!("{marker} "), style),
                Span::styled(view::truncate_line(source, diff_width), style),
            ])
        })
        .collect::<Vec<_>>();
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("{prefix}  … {hidden} more changed lines"),
            theme().dim,
        )));
    }
    lines
}

/// The command palette: filtered rows with the selection highlighted,
/// windowed to the available height.
pub(crate) fn palette_lines(app: &App, height: usize, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let query = app.palette_query().unwrap_or("");
    let filtered = filter_commands(query);
    let mut lines = Vec::new();

    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matching commands · esc to close",
            t.dim,
        )));
        return lines;
    }

    let rows = height.saturating_sub(2).max(1);
    let mut picker = app.palette_picker;
    picker.set_len(filtered.len());
    picker.windowed_table_lines(
        "Commands · type to filter · enter run · tab complete · esc close",
        filtered.into_iter().map(command_picker_row),
        [(6, 16), (12, 64), (0, 10)],
        width,
        rows,
    )
}

/// Re-render a recorded transcript into the UI: user turns carry the
/// spine, assistant text lands as markdown, and tool activity collapses
/// to the dim call/result summaries. The live activity rail is not
/// reconstructed — replay is a readable history, not a re-run.
pub(crate) fn replay_transcript(
    app: &mut App,
    messages: &[orca_harness_core::Message],
    width: usize,
) {
    for message in messages {
        match message {
            Message::System { .. } => {}
            Message::User { content, .. } => {
                app.push_line(Line::from(""));
                app.push_user_prompt(content, width);
                app.turn_count += 1;
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                if let Some(text) = content {
                    if !text.trim().is_empty() {
                        app.push_markdown_block(text, width, BlockSpacing::Section);
                    }
                }
                for call in tool_calls {
                    let line = format!("• {}", view::tool_call_line(&call.name, &call.arguments));
                    app.push_line(Line::from(Span::styled(
                        view::truncate_line(&line, width),
                        theme().dim,
                    )));
                }
            }
            Message::Tool { results } => {
                for result in results {
                    let line = format!(
                        "  {}",
                        view::tool_result_summary(
                            &result.tool_name,
                            &result.output,
                            result.is_error
                        )
                    );
                    app.push_line(Line::from(Span::styled(
                        view::truncate_line(&line, width),
                        theme().dim,
                    )));
                }
            }
        }
    }
    // Replay is history, not a live model phase: the next real turn
    // starts with a clean vertical rhythm.
    app.assistant_started = false;
}

/// Reset every piece of per-conversation UI state. Used by /clear and
/// when the worker adopts another session.
pub(crate) fn reset_conversation_ui(app: &mut App) {
    app.transcript.clear();
    app.pending_history.clear();
    app.scroll = 0;
    app.transcript_max_scroll = 0;
    app.split_inspector_cache = None;
    app.ask = None;
    app.prompt_queue.clear();
    app.tokens_in = 0;
    app.tokens_out = 0;
    app.cache_read_total = 0;
    app.cache_write_total = 0;
    app.usage_steps = 0;
    app.context_tokens = 0;
    app.tool_log.clear();
    app.work_log.clear();
    app.turn_count = 0;
    app.reset_activity();
    app.split_snapshot = None;
    // The answer is gone from the transcript; leaving it copyable would
    // hand back content the user just asked to be rid of.
    app.last_answer = None;
}
pub(crate) fn model_picker_lines(
    picker: &ModelPicker,
    height: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let t = theme();
    let filtered = picker.filtered();
    let mut lines = Vec::new();

    let filter_note = if picker.filter.is_empty() {
        "type to filter".to_string()
    } else {
        format!("filter: {}", picker.filter)
    };
    let header_left = format!(
        "  Models {} · {filter_note} · enter switch · esc close",
        filtered.len()
    );
    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(header_left, t.dim)));
        lines.push(Line::from(Span::styled(
            "  no models match — backspace to widen",
            t.dim,
        )));
        return lines;
    }

    let rows = height.saturating_sub(2).max(1);
    picker.picker.windowed_table_lines(
        &format!("Models · {filter_note} · enter switch · esc close"),
        filtered.into_iter().map(|model| {
            let summary = model.summary();
            let detail = summary
                .strip_prefix(&model.id)
                .unwrap_or_default()
                .trim()
                .to_string();
            [model.id.clone(), detail]
        }),
        [(8, 48), (0, usize::MAX)],
        width,
        rows,
    )
}

/// The status-line context segment: percentage of the model's window when
/// known (`ctx 33%`), a plain count only when no window is discoverable.
/// Exact figures live behind /usage, never here.
pub(crate) fn context_segment(tokens: u64, window: Option<u64>) -> String {
    match window {
        Some(window) if window > 0 => {
            format!("ctx {}%", (100 * tokens / window).min(999))
        }
        _ => format!("ctx ~{}", fmt_tokens(tokens)),
    }
}
