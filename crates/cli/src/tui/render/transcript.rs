use ratatui::text::{Line, Span};

use orca_harness_tools::TodoStatus;

use crate::tui::components::progress_list::{progress_list, ProgressItem, ProgressState};
use crate::tui::components::subagent_row::SubagentRow;
use crate::tui::components::tool_row::ToolRow;
use crate::tui::components::transcript::{append_block, line_is_blank, BlockSpacing};
use crate::tui::components::tree::{Connector, TreeBranch};
use crate::tui::state::{SubagentDisplay, ToolStatus};
use crate::view::glyphs::glyphs;
use crate::view::{self, theme};

use super::super::format::tool_timing_label;
use super::super::{App, LocationPicker, SkillMentionPicker, ToolActivity, PICKER_ROWS};
mod activity;
pub(crate) use activity::*;

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
        let mut lines = view::markdown_lines(answer, width, "  ");
        // The caret marks where the stream is; only while text is still
        // arriving, never on a message waiting for its result.
        let caret = glyphs().caret;
        if !caret.is_empty() && !app.text.is_empty() {
            if let Some(last) = lines.iter_mut().rev().find(|line| !line_is_blank(line)) {
                last.spans.push(Span::styled(caret, theme().accent));
            }
        }
        append_block(&mut tail, lines, BlockSpacing::Section, prior);
    }
    tail
}

pub(crate) fn identity_label(identity: &orca_harness_tools::SubagentIdentity) -> String {
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

fn subagent_display(
    app: &App,
    owner_id: Option<u64>,
    tool: &ToolActivity,
) -> Option<SubagentDisplay> {
    if let Some(owner_id) = owner_id {
        return app
            .subagent_transcripts
            .values()
            .filter(|child| child.parent_id == Some(owner_id) && child.call_id == tool.call_id)
            .max_by_key(|child| child.id)
            .and_then(|child| {
                Some(SubagentDisplay {
                    task: child.task.clone(),
                    identity: child.identity.clone()?,
                })
            });
    }
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

/// The mark and color for a tool row, from the style table. Nested and
/// top-level rows read the same function so they cannot drift apart.
fn tool_mark(tool: &ToolActivity, live: bool, frame: usize) -> (String, ratatui::style::Style) {
    let t = theme();
    let g = glyphs();
    match tool.status(live) {
        ToolStatus::Running => (g.running_frame(frame).to_string(), t.dim),
        ToolStatus::Done => (g.done.to_string(), t.success),
        ToolStatus::Failed => (g.failed.to_string(), t.error),
        ToolStatus::Abandoned => (g.failed.to_string(), t.warn),
    }
}

/// The section label mark: the live frame while the run goes, the rest
/// mark once it is committed.
fn section_mark(live: bool, frame: usize) -> String {
    let g = glyphs();
    if live {
        g.active_frame(frame).to_string()
    } else {
        g.section.to_string()
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
        let (glyph, style) = tool_mark(tool, true, app.spinner_frame);
        let glyph = glyph.as_str();
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
