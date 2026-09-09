use ratatui::text::{Line, Span};

use orca_harness_core::Message;

use crate::tui::command_catalog::filter_commands;
use crate::tui::components::transcript::BlockSpacing;
use crate::view::{self, theme};

use super::super::format::fmt_tokens;
use super::super::{App, EffortPicker, ModelPicker, ToolActivity};
use super::pickers::{command_picker_row, COMMAND_COLUMNS};
/// Exact, source-preserving mutation context beneath edit and patch rows.
/// Small changes remain fully visible; large batches stay bounded so one call
/// cannot take over the live rail.
pub(crate) fn mutation_diff_preview_lines(
    tool: &ToolActivity,
    width: usize,
    continuation: &str,
) -> Vec<Line<'static>> {
    const MAX_EDIT_DIFF_ROWS: usize = 6;

    let mut changed = Vec::new();
    match tool.tool_name.as_str() {
        "edit_file" => push_exact_edit(&mut changed, &tool.input),
        "multi_edit" => {
            if let Some(edits) = tool
                .input
                .get("edits")
                .and_then(serde_json::Value::as_array)
            {
                for edit in edits {
                    push_exact_edit(&mut changed, edit);
                }
            }
        }
        "apply_patch" => {
            if let Some(patch) = tool.input.get("patch").and_then(serde_json::Value::as_str) {
                changed.extend(patch.lines().filter_map(|line| match line.chars().next() {
                    Some('-') => Some(('-', &line[1..], theme().dim)),
                    Some('+') => Some(('+', &line[1..], theme().success)),
                    _ => None,
                }));
            }
        }
        _ => {}
    }

    let hidden = changed.len().saturating_sub(MAX_EDIT_DIFF_ROWS);
    let prefix = format!("    {continuation} ");
    let diff_width = width.saturating_sub(view::cell_width(&prefix) + 2).max(16);
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

fn push_exact_edit<'a>(
    changed: &mut Vec<(char, &'a str, ratatui::style::Style)>,
    edit: &'a serde_json::Value,
) {
    let old = edit
        .get("old")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let new = edit
        .get("new")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    changed.extend(old.lines().map(|line| ('-', line, theme().dim)));
    changed.extend(new.lines().map(|line| ('+', line, theme().success)));
    if old.lines().next().is_none()
        && new.lines().next().is_none()
        && (!old.is_empty() || !new.is_empty())
    {
        changed.push((if old.is_empty() { '+' } else { '-' }, "", theme().dim));
    }
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
    let mut picker = app.palette_picker.clone();
    picker.set_len(filtered.len());
    picker.windowed_table_lines(
        "Commands · type to filter · →/enter run · tab complete · esc close",
        filtered.into_iter().map(command_picker_row),
        COMMAND_COLUMNS,
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
    app.subagent_transcripts.clear();
    app.workflows.clear();
    app.workflow_stages.clear();
    app.evicted_agent_histories = 0;
    app.invalidate_agent_list();
    app.agent_browser = None;
    app.status_focus = None;
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
    let help = Line::from(Span::styled(
        view::truncate_line(
            "  Missing models? Update Orcacode or check account access.",
            width,
        ),
        t.dim,
    ));

    let filter_note = if picker.filter.is_empty() {
        "type to filter".to_string()
    } else {
        format!("filter: {}", picker.filter)
    };
    let title = picker
        .subagent
        .as_ref()
        .map(|(tier, provider)| {
            let tier = if tier == "flash" {
                "fast"
            } else {
                tier.as_str()
            };
            format!("Subagent {tier} / {}", provider.label())
        })
        .unwrap_or_else(|| "Models".into());
    let action = if picker.subagent.is_some() {
        "select"
    } else {
        "switch"
    };
    let header_left = format!(
        "  {title} {} · {filter_note} · →/enter {action} · esc close",
        filtered.len()
    );
    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(header_left, t.dim)));
        lines.push(Line::from(Span::styled(
            "  no models match — backspace to widen",
            t.dim,
        )));
        lines.push(help);
        return lines;
    }

    let rows = height.saturating_sub(2).max(1);
    lines = picker.picker.windowed_table_lines(
        &format!("{title} · {filter_note} · →/enter {action} · esc close"),
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
    );
    // Reuse the table spacer so help never displaces a model row.
    lines[1] = help;
    lines
}

pub(crate) fn effort_picker_lines(picker: &EffortPicker, width: usize) -> Vec<Line<'static>> {
    picker.picker.table_lines(
        &format!(
            "Reasoning effort · {} · ↑↓ move · →/enter use · ← models · esc close",
            picker.model_id
        ),
        picker.efforts.iter().map(|effort| [effort.clone()]),
        [(0, usize::MAX)],
        width,
    )
}

/// The status-line context segment: percentage of the model's window when
/// known (`ctx 33%`), a plain count only when no window is discoverable.
/// Exact figures live behind /usage, never here. The full form adds the
/// style's meter when it has one; the `compact` form is what a tight row
/// falls back to.
pub(crate) fn context_segment(tokens: u64, window: Option<u64>, compact: bool) -> String {
    let Some(window) = window.filter(|window| *window > 0) else {
        return format!("ctx ~{}", fmt_tokens(tokens));
    };
    let percent = (100 * tokens / window).min(999);
    // No meter for an empty context: a bar of nothing says nothing.
    let meter = (!compact && tokens > 0)
        .then(|| crate::view::glyphs::glyphs().meter_bar(6, tokens as f64 / window as f64))
        .flatten();
    match meter {
        // The meter is the label; the word would only repeat it.
        Some(bar) => format!("{bar} {percent}%"),
        None => format!("ctx {percent}%"),
    }
}
