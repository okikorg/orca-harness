use std::time::Duration;

use ratatui::text::{Line, Span};

use orca_harness_tools::TodoStatus;

use crate::tui::components::activity_rail::{ActivityRail, ActivityRailKind};
use crate::tui::components::progress_list::{progress_list, ProgressItem, ProgressState};
use crate::tui::components::subagent_row::SubagentRow;
use crate::tui::components::tool_row::ToolRow;
use crate::tui::components::transcript::{append_block, line_is_blank, BlockSpacing};
use crate::tui::components::tree::{Connector, TreeBranch};
use crate::tui::state::SubagentDisplay;
use crate::view::{self, theme};

use super::super::format::{elapsed_label, plural, tool_timing_label};
use super::super::{
    App, LocationPicker, SkillMentionPicker, ToolActivity, LIVE_TOOL_ROWS, PICKER_ROWS,
};
use super::overlays::*;
/// The task list belongs beside the live run state, where the complete
/// plan stays visible instead of disappearing into a clipped status line.
pub(crate) fn todo_lines(todos: &orca_harness_tools::TodoList, width: usize) -> Vec<Line<'static>> {
    let items = todos.items();
    let items: Vec<_> = items
        .iter()
        .map(|item| ProgressItem {
            content: &item.content,
            state: match item.status {
                TodoStatus::Completed => ProgressState::Completed,
                TodoStatus::InProgress => ProgressState::Active,
                TodoStatus::Pending => ProgressState::Pending,
            },
        })
        .collect();
    progress_list("todo", &items, width)
}

pub(crate) fn location_picker_lines(picker: &LocationPicker, width: usize) -> Vec<Line<'static>> {
    let filtered = picker.filtered();
    if filtered.is_empty() {
        return vec![Line::from(Span::styled(
            format!("  No workspace paths match @{} · esc close", picker.query),
            theme().dim,
        ))];
    }
    let header = if picker.query.is_empty() {
        "Files and folders · type to filter · →/enter add · esc close".to_string()
    } else {
        format!(
            "Files and folders matching @{} · →/enter add · esc close",
            picker.query
        )
    };
    picker.picker.windowed_lines(
        &header,
        filtered.into_iter().map(|entry| {
            if entry.directory {
                format!("{}/", entry.path)
            } else {
                entry.path.clone()
            }
        }),
        width,
        PICKER_ROWS,
    )
}

pub(crate) fn skill_mention_picker_lines(
    picker: &SkillMentionPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let filtered = picker.filtered();
    if filtered.is_empty() {
        return vec![Line::from(Span::styled(
            format!("  No enabled skills match ${} · esc close", picker.query),
            theme().dim,
        ))];
    }
    let header = if picker.query.is_empty() {
        "Skills · type to filter · →/enter invoke · esc close".to_string()
    } else {
        format!(
            "Skills matching ${} · →/enter invoke · esc close",
            picker.query
        )
    };
    picker.picker.windowed_table_lines(
        &header,
        filtered
            .into_iter()
            .map(|entry| [entry.name.clone(), entry.description.trim().to_string()]),
        [(4, 24), (0, usize::MAX)],
        width,
        PICKER_ROWS,
    )
}

#[cfg(test)]
pub(crate) fn projected_transcript(app: &App, width: usize) -> Vec<Line<'static>> {
    projected_transcript_selected(app, width, None)
}

/// The whole projection as one vector: committed history plus the live
/// tail. The renderer reads the two parts separately so the history is
/// not copied on every frame; tests want the joined view.
#[cfg(test)]
pub(crate) fn projected_transcript_selected(
    app: &App,
    width: usize,
    selected_tool: Option<usize>,
) -> Vec<Line<'static>> {
    let tail = projected_tail(app, width, selected_tool);
    let mut lines = committed_transcript(app, !tail.is_empty()).to_vec();
    lines.extend(tail);
    lines
}

/// The committed transcript as the renderer reads it. When a live tail
/// follows, trailing blank rows are left off so the tail's own spacing
/// rule sets the gap, exactly as `append_block` would have popped them.
pub(crate) fn committed_transcript(app: &App, tail_follows: bool) -> &[Line<'static>] {
    let lines = app.transcript.as_slice();
    if !tail_follows {
        return lines;
    }
    let end = lines
        .iter()
        .rposition(|line| !line_is_blank(line))
        .map_or(0, |index| index + 1);
    &lines[..end]
}

/// The render-only projection of the in-progress turn: live activity and
/// the streaming answer, spaced as if appended after the committed
/// transcript. Empty when nothing is running or nothing has arrived yet.
pub(crate) fn projected_tail(
    app: &App,
    width: usize,
    selected_tool: Option<usize>,
) -> Vec<Line<'static>> {
    let mut tail = Vec::new();
    if !app.running() {
        return tail;
    }
    let activity = activity_lines_selected(app, width, true, selected_tool);
    let answer = if !app.text.is_empty() {
        Some(app.text.as_str())
    } else {
        app.pending_assistant
            .as_deref()
            .filter(|text| !text.is_empty())
    };
    // What the committed transcript ends with, once its trailing blank
    // rows are ignored: a row of content, or nothing at all.
    let prior = (!committed_transcript(app, true).is_empty()).then_some(false);
    append_block(&mut tail, activity, BlockSpacing::Section, prior);
    if let Some(answer) = answer {
        append_block(
            &mut tail,
            view::markdown_lines(answer, width, "  "),
            BlockSpacing::Section,
            prior,
        );
    }
    tail
}

fn identity_label(identity: &orca_harness_tools::SubagentIdentity) -> String {
    format!("{}:{}", identity.provider, identity.model)
}

fn result_identity(output: &serde_json::Value) -> Option<orca_harness_tools::SubagentIdentity> {
    let identity = output.get("identity")?;
    Some(orca_harness_tools::SubagentIdentity {
        provider: identity.get("provider")?.as_str()?.to_string(),
        model: identity.get("model")?.as_str()?.to_string(),
        route: identity
            .get("route")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

fn subagent_display(app: &App, tool: &ToolActivity) -> Option<SubagentDisplay> {
    app.subagent_display
        .get(&tool.call_id)
        .map(|display| SubagentDisplay {
            task: display.task.clone(),
            identity: display.identity.clone(),
        })
        .or_else(|| {
            Some(SubagentDisplay {
                task: tool.input.get("task")?.as_str()?.to_string(),
                identity: result_identity(tool.output.as_ref()?)?,
            })
        })
}

/// Cap on rendered inner tool rows per spawn while live.
const NESTED_TOOL_ROWS: usize = 4;

/// Inner tool rows for every spawn anchored to `call_id`, plus their
/// descendants. Rows reuse the rail's `├─`/`└─` vocabulary one level
/// deeper, and `continuation` carries the parent rail's `│ ` (or blank)
/// so ownership stays unambiguous even mid-list.
pub(crate) fn nested_subagent_lines(
    app: &App,
    call_id: &str,
    width: usize,
    continuation: &str,
    lines: &mut Vec<Line<'static>>,
) {
    let roots: Vec<u64> = app
        .subagent_activity
        .iter()
        .filter(|(_, spawn)| spawn.parent_id.is_none() && spawn.call_id == call_id)
        .map(|(id, _)| *id)
        .max()
        .into_iter()
        .collect();
    let indent = format!("    {continuation}");
    for id in roots {
        nested_spawn_rows(app, id, width, &indent, lines);
    }
}

pub(crate) fn nested_spawn_rows(
    app: &App,
    id: u64,
    width: usize,
    indent: &str,
    lines: &mut Vec<Line<'static>>,
) {
    let Some(spawn) = app.subagent_activity.get(&id) else {
        return;
    };
    let t = theme();
    let hidden = spawn.tools.len().saturating_sub(NESTED_TOOL_ROWS);
    if hidden > 0 {
        lines.push(hidden_tools_line(indent, hidden));
    }
    let visible: Vec<&ToolActivity> = spawn.tools.iter().skip(hidden).collect();
    for (position, tool) in visible.iter().enumerate() {
        let branch = TreeBranch {
            indent,
            last: position + 1 == visible.len(),
        };
        let elapsed = tool_timing_label(tool, false);
        let (glyph, style) = match (&tool.output, tool.elapsed) {
            (Some(_), _) if tool.is_error => ("×", t.error),
            (Some(_), _) => ("✓", t.dim),
            (None, Some(_)) if tool.is_error => ("×", t.error),
            (None, Some(_)) => ("✓", t.dim),
            (None, None) => ("□", t.dim),
        };
        let child = (tool.tool_name == "subagent")
            .then(|| {
                app.subagent_activity
                    .iter()
                    .filter(|(_, child)| {
                        child.parent_id == Some(id) && child.call_id == tool.call_id
                    })
                    .max_by_key(|(child_id, _)| *child_id)
            })
            .flatten();
        if let Some((_, child)) = child.filter(|(_, child)| child.identity.is_some()) {
            let identity = identity_label(child.identity.as_ref().unwrap());
            lines.push(
                SubagentRow {
                    branch,
                    glyph,
                    identity: &identity,
                    task: &child.task,
                    elapsed: &elapsed,
                    connector: Connector::None,
                    width,
                    branch_style: t.dim,
                    glyph_style: style,
                    label_style: t.accent,
                    identity_style: t.accent,
                    task_style: t.accent,
                }
                .line(),
            );
        } else {
            lines.push(
                ToolRow {
                    branch,
                    glyph,
                    call: &tool.call_line,
                    detail: "",
                    elapsed: &elapsed,
                    connector: Connector::None,
                    width,
                    branch_style: t.dim,
                    glyph_style: style,
                    call_style: t.accent,
                }
                .line(),
            );
        }
        if tool.tool_name == "subagent" && tool.output.is_none() {
            if let Some((child_id, _)) = child {
                nested_spawn_rows(app, *child_id, width, &branch.child_indent(), lines);
            }
        }
    }
}

/// The "… n earlier tools" row a live rail shows for tools it has scrolled
/// past; the same row at every nesting depth.
fn hidden_tools_line(indent: &str, hidden: usize) -> Line<'static> {
    Line::from(Span::styled(
        format!("{indent}… {hidden} earlier tools"),
        theme().dim,
    ))
}

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
            "•",
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
        ActivityRail::new(ActivityRailKind::Work, "•", parts.join(" · "), style)
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
    let t = theme();
    let mut lines = Vec::new();
    let current_thinking = !app.reasoning.trim().is_empty();
    let thinking_count = app.thinking_log.len() + usize::from(current_thinking);
    if thinking_count > 0 {
        let elapsed = app
            .thinking_log
            .iter()
            .map(|record| record.elapsed)
            .sum::<Duration>()
            + app
                .reasoning_started
                .map(|started| started.elapsed())
                .unwrap_or_default();
        let marker = if live {
            if app.spinner_frame.is_multiple_of(2) {
                "•"
            } else {
                " "
            }
        } else {
            "•"
        };
        let mut thinking = ActivityRail::new(
            ActivityRailKind::Thinking,
            marker,
            format!(
                "{} · {}",
                elapsed_label(elapsed),
                plural(thinking_count, "update")
            ),
            t.dim,
        );
        if live && current_thinking {
            let body_width = width.saturating_sub(6).max(16);
            let wrapped: Vec<String> = app
                .reasoning
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

    if app.activity_tools.is_empty() {
        return lines;
    }
    let complete = app
        .activity_tools
        .iter()
        .filter(|tool| tool.elapsed.is_some())
        .count();
    let running = app.activity_tools.len() - complete;
    let (marker, summary) = if live {
        let dot = if app.spinner_frame.is_multiple_of(2) {
            "•"
        } else {
            " "
        };
        (dot, format!("✓ {complete} · □ {running}"))
    } else {
        ("•", plural(app.activity_tools.len(), "tool"))
    };
    let mut work = ActivityRail::new(ActivityRailKind::Work, marker, summary, t.dim);

    let visible_indices = if live && app.activity_tools.len() > LIVE_TOOL_ROWS {
        let mut selected: Vec<usize> = app
            .activity_tools
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, tool)| tool.elapsed.is_none())
            .map(|(index, _)| index)
            .take(LIVE_TOOL_ROWS)
            .collect();
        let remaining = LIVE_TOOL_ROWS.saturating_sub(selected.len());
        selected.extend(
            app.activity_tools
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
        (0..app.activity_tools.len()).collect()
    };
    let hidden = app
        .activity_tools
        .len()
        .saturating_sub(visible_indices.len());
    if hidden > 0 {
        work.push(hidden_tools_line("    ", hidden));
    }

    for (position, index) in visible_indices.iter().copied().enumerate() {
        let tool = &app.activity_tools[index];
        let branch = TreeBranch {
            indent: "    ",
            last: position + 1 == visible_indices.len(),
        };
        let (glyph, mut detail, status_style) = match &tool.output {
            Some(output) if tool.is_error => (
                "×",
                view::tool_result_summary(&tool.tool_name, output, true),
                t.error,
            ),
            Some(output) => (
                "✓",
                view::tool_result_summary(&tool.tool_name, output, false),
                t.dim,
            ),
            None if tool.elapsed.is_some() && tool.is_error => ("×", String::new(), t.error),
            None if tool.elapsed.is_some() => ("✓", String::new(), t.dim),
            None if live => ("□", String::new(), t.dim),
            None => ("×", String::new(), t.warn),
        };
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
            if let Some(display) = subagent_display(app, tool) {
                let identity = identity_label(&display.identity);
                let row = SubagentRow {
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
        if tool.tool_name == "subagent" && tool.output.is_none() {
            let mut nested = Vec::new();
            nested_subagent_lines(app, &tool.call_id, width, continuation, &mut nested);
            work.extend(nested);
        }
        if tool.is_error {
            if let Some(output) = &tool.output {
                let output_width = width.saturating_sub(12).max(16);
                for output_line in view::expand_output(&tool.tool_name, output)
                    .into_iter()
                    .take(4)
                {
                    work.push(Line::from(Span::styled(
                        format!(
                            "    {continuation} │ {}",
                            view::truncate_line(&output_line, output_width)
                        ),
                        t.error,
                    )));
                }
            }
        }
    }
    work.append_to(&mut lines);
    lines
}
