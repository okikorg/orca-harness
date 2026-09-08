//! Shared activity-rail projections for parent and child agents.

use std::time::Duration;

use ratatui::text::{Line, Span};

use crate::tui::components::activity_rail::{ActivityRail, ActivityRailKind};
use crate::tui::components::subagent_row::SubagentRow;
use crate::tui::components::tool_row::ToolRow;
use crate::tui::components::tree::{Connector, TreeBranch};
use crate::tui::format::{elapsed_label, plural, tool_timing_label};
use crate::tui::state::{SubagentTranscript, ThinkingRecord};
use crate::tui::{App, ToolActivity, LIVE_TOOL_ROWS};
use crate::view::glyphs::glyphs;
use crate::view::{self, theme};

use super::super::overlays::mutation_diff_preview_lines;
use super::{
    hidden_tools_line, identity_label, nested_subagent_lines, section_mark, subagent_display,
    tool_mark,
};

/// Quiet, chronological rows for a completed phase. Thinking and tool work
/// remain separate so collapsing detail never rewrites the event sequence.
pub(crate) fn collapsed_activity_lines(app: &App) -> Vec<Line<'static>> {
    let tool_count = app.activity_tools.len();
    let thinking_count = app.thinking_log.len();
    let failed = app
        .activity_tools
        .iter()
        .filter(|tool| tool.is_error)
        .count();
    let tool_elapsed = app
        .activity_tools
        .iter()
        .filter_map(|tool| tool.elapsed)
        .max()
        .unwrap_or_default();

    let mut lines = Vec::new();
    if thinking_count > 0 {
        let thinking_elapsed = app
            .thinking_log
            .iter()
            .map(|record| record.elapsed)
            .sum::<Duration>();
        ActivityRail::new(
            ActivityRailKind::Thinking,
            &section_mark(false, 0),
            format!(
                "{} · {}",
                elapsed_label(thinking_elapsed),
                plural(thinking_count, "update")
            ),
            theme().dim,
        )
        .append_to(&mut lines);
    }

    if tool_count > 0 {
        let mut parts = vec![plural(tool_count, "tool")];
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        parts.push(elapsed_label(tool_elapsed));
        let style = if failed > 0 {
            theme().error
        } else {
            theme().dim
        };
        ActivityRail::new(
            ActivityRailKind::Work,
            &section_mark(false, 0),
            parts.join(" · "),
            style,
        )
        .append_to(&mut lines);
    }

    lines
}

/// Render the current run as one coherent activity rail. While the run is
/// live this includes the latest reasoning tail and pending tool states;
/// once committed, the rail is retained for on-demand expansion.
#[cfg(test)]
pub(crate) fn activity_lines(app: &App, width: usize, live: bool) -> Vec<Line<'static>> {
    activity_lines_selected(app, width, live, None)
}

pub(crate) fn activity_lines_selected(
    app: &App,
    width: usize,
    live: bool,
    selected_tool: Option<usize>,
) -> Vec<Line<'static>> {
    activity_lines_from(
        app,
        None,
        &app.reasoning,
        app.reasoning_started,
        &app.thinking_log,
        &app.activity_tools,
        app.spinner_frame,
        width,
        live,
        selected_tool,
    )
}

/// Project a child agent's work with the same thinking/work rails, tool rows,
/// timing, result summaries, errors, and mutation previews as the parent.
pub(crate) fn subagent_activity_lines(
    app: &App,
    owner_id: u64,
    thinking_log: &[ThinkingRecord],
    tools: &[ToolActivity],
    width: usize,
) -> Vec<Line<'static>> {
    activity_lines_from(
        app,
        Some(owner_id),
        "",
        None,
        thinking_log,
        tools,
        app.spinner_frame,
        width,
        false,
        None,
    )
}

pub(crate) fn live_subagent_activity_lines(
    app: &App,
    transcript: &SubagentTranscript,
    width: usize,
) -> Vec<Line<'static>> {
    activity_lines_from(
        app,
        Some(transcript.id),
        &transcript.streaming_reasoning,
        transcript.reasoning_started,
        &transcript.thinking_log,
        &transcript.activity_tools,
        app.spinner_frame,
        width,
        true,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn activity_lines_from(
    app: &App,
    owner_id: Option<u64>,
    reasoning: &str,
    reasoning_started: Option<std::time::Instant>,
    thinking_log: &[ThinkingRecord],
    tools: &[ToolActivity],
    spinner_frame: usize,
    width: usize,
    live: bool,
    selected_tool: Option<usize>,
) -> Vec<Line<'static>> {
    let t = theme();
    let mut lines = Vec::new();
    let current_thinking = !reasoning.trim().is_empty();
    let thinking_count = thinking_log.len() + usize::from(current_thinking);
    if thinking_count > 0 {
        let elapsed = thinking_log
            .iter()
            .map(|record| record.elapsed)
            .sum::<Duration>()
            + reasoning_started
                .map(|started| started.elapsed())
                .unwrap_or_default();
        // Thinking is at rest once a tool call or answer has followed it.
        let thinking_live = live && current_thinking;
        let mut thinking = ActivityRail::new(
            ActivityRailKind::Thinking,
            &section_mark(thinking_live, spinner_frame),
            format!(
                "{} · {}",
                elapsed_label(elapsed),
                plural(thinking_count, "update")
            ),
            t.dim,
        );
        if live && current_thinking {
            let body_width = width.saturating_sub(6).max(16);
            let wrapped: Vec<String> = reasoning
                .lines()
                .flat_map(|paragraph| {
                    textwrap::wrap(paragraph, body_width)
                        .into_iter()
                        .map(|part| part.into_owned())
                })
                .collect();
            for line in wrapped.iter().rev().take(2).rev() {
                thinking.push(Line::from(Span::styled(format!("    {line}"), t.dim)));
            }
        }
        thinking.append_to(&mut lines);
    }

    if tools.is_empty() {
        return lines;
    }
    let complete = tools.iter().filter(|tool| tool.elapsed.is_some()).count();
    let running = tools.len() - complete;
    let g = glyphs();
    let (marker, summary) = if live {
        (
            section_mark(running > 0, spinner_frame),
            format!("{} {complete} · {} {running}", g.done, g.waiting),
        )
    } else {
        (section_mark(false, 0), plural(tools.len(), "tool"))
    };
    let mut work = ActivityRail::new(ActivityRailKind::Work, &marker, summary, t.dim);

    let visible_indices = if live && tools.len() > LIVE_TOOL_ROWS {
        let mut selected: Vec<usize> = tools
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, tool)| tool.elapsed.is_none())
            .map(|(index, _)| index)
            .take(LIVE_TOOL_ROWS)
            .collect();
        let remaining = LIVE_TOOL_ROWS.saturating_sub(selected.len());
        selected.extend(
            tools
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, tool)| tool.elapsed.is_some())
                .map(|(index, _)| index)
                .take(remaining),
        );
        selected.sort_unstable();
        selected
    } else {
        (0..tools.len()).collect()
    };
    let hidden = tools.len().saturating_sub(visible_indices.len());
    if hidden > 0 {
        work.push(hidden_tools_line("    ", hidden));
    }

    for (position, index) in visible_indices.iter().copied().enumerate() {
        let tool = &tools[index];
        let branch = TreeBranch {
            indent: "    ",
            last: position + 1 == visible_indices.len(),
        };
        let (glyph, status_style) = tool_mark(tool, live, spinner_frame);
        let glyph = glyph.as_str();
        let mut detail = tool
            .output
            .as_ref()
            .filter(|_| !tool.is_error)
            .map(|output| view::tool_result_summary(&tool.tool_name, output, false))
            .unwrap_or_default();
        if let Some(approval) = &tool.approval {
            detail = if detail.is_empty() {
                approval.clone()
            } else {
                format!("{approval} · {detail}")
            };
        }
        let elapsed = tool_timing_label(tool, false);
        let selected = selected_tool == Some(index);
        let connector = Connector::for_row(selected_tool.is_some(), selected);
        let row_style = if selected { t.select } else { t.accent };
        let continuation = if tool.tool_name == "subagent" {
            if let Some(display) = subagent_display(app, owner_id, tool) {
                let identity = identity_label(&display.identity);
                let row = SubagentRow {
                    label: "Subagent",
                    branch,
                    glyph,
                    identity: &identity,
                    task: &display.task,
                    elapsed: &elapsed,
                    connector,
                    width,
                    branch_style: t.dim,
                    glyph_style: status_style,
                    label_style: row_style,
                    identity_style: t.accent,
                    task_style: row_style,
                };
                let continuation = row.continuation();
                work.push(row.line());
                continuation
            } else {
                let row = ToolRow {
                    branch,
                    glyph,
                    call: &tool.call_line,
                    detail: &detail,
                    elapsed: &elapsed,
                    connector,
                    width,
                    branch_style: t.dim,
                    glyph_style: status_style,
                    call_style: row_style,
                };
                let continuation = row.continuation();
                work.push(row.line());
                continuation
            }
        } else {
            let row = ToolRow {
                branch,
                glyph,
                call: &tool.call_line,
                detail: &detail,
                elapsed: &elapsed,
                connector,
                width,
                branch_style: t.dim,
                glyph_style: status_style,
                call_style: row_style,
            };
            let continuation = row.continuation();
            work.push(row.line());
            continuation
        };
        if matches!(
            tool.tool_name.as_str(),
            "edit_file" | "multi_edit" | "apply_patch"
        ) {
            work.extend(mutation_diff_preview_lines(tool, width, continuation));
        }
        if owner_id.is_none() && tool.tool_name == "subagent" && tool.output.is_none() {
            let mut nested = Vec::new();
            nested_subagent_lines(app, &tool.call_id, width, continuation, &mut nested);
            work.extend(nested);
        }
        if tool.is_error {
            if let Some(output) = &tool.output {
                work.extend(crate::tui::components::tool_error::lines(
                    output,
                    width,
                    continuation,
                ));
            }
        }
    }
    work.append_to(&mut lines);
    lines
}
