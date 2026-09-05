//! The agent browser: every spawned agent as a picker over the spawn tree,
//! beside the selected agent's transcript. The panes borrow split view's
//! geometry and chrome, the list is the standard picker tray under a
//! Running / Done / Failed / All tab strip, and the transcript is the same
//! Thinking/Work rails and markdown as the parent's.

use std::sync::Arc;
use std::time::Duration;

use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::components::picker::ListPicker;
use crate::tui::components::status_bar::{self, Segment, StatusBar};
use crate::tui::components::tabs::tab_strip;
use crate::tui::components::transcript::{append_block, line_is_blank, BlockSpacing};
use crate::tui::format::{elapsed_label, fmt_tokens};
use crate::tui::state::{
    AgentBodyCache, AgentTab, App, SubagentTranscript, SubagentTranscriptEntry,
    SubagentTranscriptStatus,
};
use crate::view::glyphs::glyphs;
use crate::view::{self, theme};

use super::shell::{shorter_hint, SplitKind, SPLIT_MIN_WIDTH};
use super::transcript::{identity_label, live_subagent_activity_lines, subagent_activity_lines};

/// Share of the width (or height, stacked) the spawn tree takes; the
/// transcript gets the rest, as the inspector does in split view.
const LIST_PERCENT: u16 = 38;
const LIST_HINT: &str = "tab switch · ↑↓ select · esc close";
const HINT: &str = "pgup/pgdn transcript · ctrl+y copy answer · esc close";

mod list;
#[cfg(test)]
pub(crate) use list::agent_tree_rows;
use list::{agent_browser_rows, agent_list_projection, agent_rows_on, agent_table};
pub(crate) use list::{agent_ids, AgentListCache, AgentTableCache};

/// Tab: the next tab, with the cursor kept on the same agent when that
/// agent is still listed, else on the first row.
pub(crate) fn cycle_agent_tab(app: &mut App) {
    let Some(browser) = app.agent_browser.as_ref() else {
        return;
    };
    let selected = agent_ids(app).get(browser.picker.index()).copied();
    let tab = browser.tab.next();
    let ids: Vec<u64> = agent_rows_on(app, tab)
        .into_iter()
        .map(|row| row.id)
        .collect();
    let index = selected
        .and_then(|id| ids.iter().position(|candidate| *candidate == id))
        .unwrap_or(0);
    if let Some(browser) = app.agent_browser.as_mut() {
        browser.tab = tab;
        browser.picker = ListPicker::with_selected(ids.len(), index);
        browser.scroll = 0;
    }
}

pub(crate) fn selected_agent_copy(app: &App) -> Option<String> {
    let browser = app.agent_browser.as_ref()?;
    let id = *agent_ids(app).get(browser.picker.index())?;
    app.subagent_transcripts
        .get(&id)?
        .latest_answer()
        .map(str::to_string)
}

pub(crate) fn draw_agent_browser(frame: &mut Frame, app: &mut App) {
    let t = theme();
    let area = frame.area();
    let [content_area, status_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    let rows = agent_browser_rows(app);
    let Some(browser) = app.agent_browser.as_mut() else {
        return;
    };
    browser.picker.set_len(rows.len());
    let selected = browser.picker.index();

    let split = if content_area.width as usize >= SPLIT_MIN_WIDTH {
        SplitKind::SideBySide
    } else {
        SplitKind::Stacked
    };
    let [list_area, transcript_area] = split.areas_at(content_area, LIST_PERCENT);

    // The list: one picker row per agent, its branch and state mark ahead
    // of the task, with the state and elapsed time as the dim trailing cell.
    let projection = agent_list_projection(app);
    let list_width = list_area.width.saturating_sub(1) as usize;
    let list_rows = agent_table(app, &projection, &rows, list_width);

    // The transcript: a pinned header, then the body anchored to its tail
    // the way the parent transcript is, scrolled back by page keys.
    let transcript_width = split.content_width(transcript_area);
    let transcript = rows
        .get(selected)
        .and_then(|row| app.subagent_transcripts.get(&row.id));
    let header = transcript.map_or_else(empty_transcript_lines, |transcript| {
        transcript_header_lines(transcript, transcript_width)
    });
    let selected_id = transcript.map(|transcript| transcript.id);
    let body = cached_transcript_body(app, selected_id, transcript_width);
    let body_height =
        (split.block(t.dim).inner(transcript_area).height as usize).saturating_sub(header.len());

    let browser = app
        .agent_browser
        .as_mut()
        .expect("agent browser remains open while drawing");
    let labels = AgentTab::ALL.map(AgentTab::label);
    let active_tab = AgentTab::ALL
        .iter()
        .position(|tab| *tab == browser.tab)
        .unwrap_or(0);
    let list_lines = browser.picker.cached_table_lines(
        tab_strip(&labels, active_tab, LIST_HINT),
        &list_rows,
        // One cell clear before the divider.
        list_area.width.saturating_sub(1) as usize,
        // Header and spacer take the first two rows.
        (list_area.height as usize).saturating_sub(2),
    );
    browser.scroll = browser.scroll.min(body.len().saturating_sub(body_height));
    let end = body.len().saturating_sub(browser.scroll);
    let start = end.saturating_sub(body_height);
    let mut pane = header;
    pane.extend(body[start..end].iter().cloned());

    frame.render_widget(Paragraph::new(Text::from(list_lines)), list_area);
    frame.render_widget(
        Paragraph::new(Text::from(pane)).block(split.block(t.dim)),
        transcript_area,
    );

    let [queued, active, _, failed] = projection.counts;
    let mut status = StatusBar::new();
    status
        .push(Segment::new(
            format!("agents {}", rows.len()),
            status_bar::KEEP,
        ))
        .push(Segment::new(format!("active {active}"), status_bar::KEEP));
    if queued > 0 {
        status.push(Segment::new(format!("queued {queued}"), status_bar::COUNTS).with_style(t.dim));
    }
    if failed > 0 {
        status
            .push(Segment::new(format!("failed {failed}"), status_bar::COUNTS).with_style(t.error));
    }
    if app.evicted_agent_histories > 0 {
        status.push(Segment::new(
            format!("{} older agents omitted", app.evicted_agent_histories),
            status_bar::COUNTS,
        ));
    }
    status
        .push(Segment::new("parent continues", status_bar::STATS))
        .push(Segment::new(HINT, status_bar::HINT).with_compact(shorter_hint(HINT)));
    frame.render_widget(
        Paragraph::new(status.line(area.width as usize, t.dim)),
        status_area,
    );
}

/// Active transcripts retain live timing; terminal transcripts reuse their expensive
/// markdown and tool projections across spinner ticks and viewport scrolling.
fn cached_transcript_body(app: &mut App, id: Option<u64>, width: usize) -> Arc<Vec<Line<'static>>> {
    let Some(transcript) = id.and_then(|id| app.subagent_transcripts.get(&id)) else {
        if let Some(browser) = &mut app.agent_browser {
            browser.body_cache = None;
        }
        return Arc::new(Vec::new());
    };
    let theme = view::theme_name();
    let style = crate::view::glyphs::ui_style();
    if !transcript.status.is_active() {
        if let Some(cache) = app
            .agent_browser
            .as_ref()
            .and_then(|browser| browser.body_cache.as_ref())
        {
            if cache.id == transcript.id
                && cache.revision == transcript.revision
                && cache.width == width
                && cache.theme == theme
                && cache.style == style
            {
                return Arc::clone(&cache.lines);
            }
        }
    }
    let lines = Arc::new(transcript_body_lines(app, transcript, width));
    let cache = (!transcript.status.is_active()).then(|| AgentBodyCache {
        id: transcript.id,
        revision: transcript.revision,
        width,
        theme,
        style,
        lines: Arc::clone(&lines),
    });
    if let Some(browser) = &mut app.agent_browser {
        browser.body_cache = cache;
    }
    lines
}

fn status_label(status: SubagentTranscriptStatus) -> &'static str {
    match status {
        SubagentTranscriptStatus::Queued => "queued",
        SubagentTranscriptStatus::Running => "running",
        SubagentTranscriptStatus::Completed => "done",
        SubagentTranscriptStatus::Failed => "failed",
    }
}

/// An agent's state mark, from the same table as tool rows so a finished
/// agent reads like a finished tool.
fn status_mark(status: SubagentTranscriptStatus) -> char {
    let g = glyphs();
    match status {
        SubagentTranscriptStatus::Queued | SubagentTranscriptStatus::Running => g.waiting,
        SubagentTranscriptStatus::Completed => g.done,
        SubagentTranscriptStatus::Failed => g.failed,
    }
}

fn status_style(status: SubagentTranscriptStatus) -> Style {
    let t = theme();
    match status {
        SubagentTranscriptStatus::Queued => t.dim,
        SubagentTranscriptStatus::Running => t.accent,
        SubagentTranscriptStatus::Completed => t.success,
        SubagentTranscriptStatus::Failed => t.error,
    }
}

fn transcript_elapsed(transcript: &SubagentTranscript) -> Duration {
    transcript
        .elapsed
        .unwrap_or_else(|| transcript.started.elapsed())
}

/// The pinned pane header, shaped like the tool inspector's: identity,
/// state and timing in the state colour, the task as the title, and the
/// dim figures under it. A blank row separates it from the body.
fn transcript_header_lines(transcript: &SubagentTranscript, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let body_width = width.saturating_sub(2);
    let identity = transcript
        .identity
        .as_ref()
        .map(identity_label)
        .unwrap_or_else(|| "inherited model".into());
    let mut figures = vec![
        format!("input {}", fmt_tokens(transcript.input_tokens)),
        format!("output {}", fmt_tokens(transcript.output_tokens)),
        format!("depth {}", transcript.depth),
    ];
    if transcript.detached {
        figures.push("background".into());
    }
    if transcript.omitted_activity > 0 {
        figures.push(format!(
            "{} earlier history items omitted",
            transcript.omitted_activity
        ));
    }
    vec![
        Line::from(Span::styled(
            format!(
                "  {}",
                view::truncate_line(
                    &format!(
                        "{} subagent · {identity} · {} · {}",
                        status_mark(transcript.status),
                        status_label(transcript.status),
                        elapsed_label(transcript_elapsed(transcript))
                    ),
                    body_width
                )
            ),
            status_style(transcript.status),
        )),
        Line::from(Span::styled(
            format!("  {}", view::truncate_line(&transcript.task, body_width)),
            t.strong,
        )),
        Line::from(Span::styled(
            format!(
                "  {}",
                view::truncate_line(&figures.join(" · "), body_width)
            ),
            t.dim,
        )),
        Line::from(""),
    ]
}

fn empty_transcript_lines() -> Vec<Line<'static>> {
    let t = theme();
    vec![
        Line::from(Span::styled("  Agents", t.strong)),
        Line::from(""),
        Line::from(Span::styled(
            "  Spawned agents and their transcripts will appear here.",
            t.dim,
        )),
    ]
}

fn transcript_body_lines(
    app: &App,
    transcript: &SubagentTranscript,
    width: usize,
) -> Vec<Line<'static>> {
    let t = theme();
    let mut lines = Vec::new();
    let mut prior = None;
    for entry in &transcript.entries {
        let block = match entry {
            SubagentTranscriptEntry::Assistant(text) => view::markdown_lines(text, width, "  "),
            SubagentTranscriptEntry::Activity { thinking, tools } => {
                subagent_activity_lines(app, transcript.id, thinking, tools, width)
            }
            SubagentTranscriptEntry::Error(message) => vec![Line::from(Span::styled(
                format!("  failed · {message}"),
                t.error,
            ))],
        };
        append_block(&mut lines, block, BlockSpacing::Section, prior);
        prior = lines.last().map(line_is_blank);
    }

    if transcript.status == SubagentTranscriptStatus::Queued {
        lines.push(Line::from(Span::styled(
            "  Waiting for a slot: the /subagents background agents limit is reached.",
            t.dim,
        )));
    }
    if transcript.status == SubagentTranscriptStatus::Running {
        let activity = live_subagent_activity_lines(app, transcript, width);
        append_block(&mut lines, activity, BlockSpacing::Section, prior);
        if !transcript.streaming_assistant.trim().is_empty() {
            let mut answer = view::markdown_lines(&transcript.streaming_assistant, width, "  ");
            let caret = glyphs().caret;
            if !caret.is_empty() {
                if let Some(last) = answer.iter_mut().rev().find(|line| !line.spans.is_empty()) {
                    last.spans.push(Span::styled(caret, t.accent));
                }
            }
            append_block(&mut lines, answer, BlockSpacing::Section, prior);
        }
    }
    lines
}
