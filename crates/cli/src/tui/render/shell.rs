// Rendering: builds the terminal line models — the welcome screen, the
// transcript/tool rails, the live region (queue/working/spinner), status
// segments, and every picker overlay. Pure layout: reads `App` state and
// returns `Vec<Line>`; the terminal loop in the parent `run`/`draw`
// drives the actual frames.

use crate::msg::ProviderExt as _;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::components::approval::ApprovalPrompt;
use crate::tui::components::composer::{command_range, Composer};
use crate::tui::components::keys;
use crate::tui::components::status_bar::{self, Segment, StatusBar};
use crate::tui::components::tabs::tab_strip;
use crate::tui::components::welcome::Welcome;
use crate::view::glyphs::glyphs;
use crate::view::theme;

use super::super::inspector::{
    empty_tool_inspector_lines, tool_inspector_body_lines, tool_inspector_header_lines,
};
use super::super::state::SplitTab;
use super::super::{App, InspectorBodyCache, StatusFocus, ViewMode};
use super::agents::draw_agent_browser;
use super::overlays::{context_segment, context_spans};
use super::transcript::*;

mod live;
mod split;
pub(crate) use live::*;
pub(crate) use split::*;

#[cfg(test)]
mod tests;

/// Rows the layout keeps for the conversation around the live region:
/// three transcript rows, the composer gap and the status line.
const RESERVED_ROWS: usize = 5;

pub(crate) fn draw(frame: &mut Frame, app: &mut App) {
    if app.agent_browser.is_some() {
        draw_agent_browser(frame, app);
        return;
    }
    let width = frame.area().width as usize;
    // Split the whole terminal first so the transcript, live rail, composer,
    // and status share one column and the inspector owns the rest.
    let split = SplitKind::for_area(app.view_mode, width, frame.area().height as usize);
    let split_active = split != SplitKind::Off;
    let [left_root, inspector_area] = split.areas(frame.area());
    app.inspector_area = split_active.then_some(inspector_area);
    let left_width = left_root.width as usize;

    let placeholder = if app.approval.is_some() {
        "answering approval above"
    } else if app.ask.is_some() {
        "answering agent clarification above"
    } else if app.running() {
        "type another prompt to queue"
    } else if !app.prompt_queue.is_empty() {
        "queue paused · enter to resume"
    } else {
        "ask anything · @ add files · /help commands"
    };
    let pill_spans = super::super::input::image_marker_spans(&app.pastes, &app.composer);
    // The spine says what enter will do: queue while a turn runs, send
    // without a check in yolo, send otherwise.
    let spine = if app.running() {
        theme().dim
    } else if app.cfg.mode.get() == crate::mode::Mode::Yolo {
        theme().warn
    } else {
        theme().accent
    };
    let composer = Composer::new(
        &app.composer,
        app.cursor,
        placeholder,
        left_width,
        theme().accent,
        theme().dim,
        &pill_spans,
    )
    .spine(spine)
    .command(command_range(&app.composer, |word| {
        super::super::command_catalog::COMMANDS
            .iter()
            .any(|spec| spec.name == word)
    }))
    .render();
    let composer_height = composer.lines.len();

    let live = live_region(app, left_width);
    // Let a todo rail use the available height rather than silently
    // clipping later steps, while keeping the reserved rows and the
    // composer on short terminals.
    let live_height = live
        .lines
        .len()
        .min((left_root.height as usize).saturating_sub(RESERVED_ROWS + composer_height));
    let [transcript_area, live_area, _composer_gap_area, composer_area, status_area] =
        Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(live_height as u16),
            Constraint::Length(1),
            Constraint::Length(composer_height as u16),
            Constraint::Length(1),
        ])
        .areas(left_root);

    // Transcript: committed history plus a render-only projection of the
    // in-progress turn. Deltas therefore appear in their final location
    // instead of streaming through the temporary area and jumping here.
    // The committed part is read in place; only the live tail is built
    // per frame.
    // A split gives the transcript pane a header to balance the
    // inspector's, so both halves read as panes.
    let transcript_area = if split_active {
        let [header_area, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(transcript_area);
        let turn = app.turn_count + usize::from(app.running());
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  transcript · turn {turn}"),
                theme().dim,
            ))),
            header_area,
        );
        rest
    } else {
        transcript_area
    };
    let height = transcript_area.height as usize;
    let transcript_width = transcript_area.width as usize;
    let selected_tool = (split_active && !app.activity_tools.is_empty()).then(|| {
        app.split_tool
            .unwrap_or_else(|| app.activity_tools.len().saturating_sub(1))
            .min(app.activity_tools.len().saturating_sub(1))
    });
    let tail = projected_tail(app, transcript_width, selected_tool);
    let projected_len = committed_transcript(app, !tail.is_empty()).len() + tail.len();
    // Connector startup notices are transcript history, but they should not
    // displace the empty-state welcome before the user begins a conversation.
    // Keep them recorded in the background and reveal the transcript on the
    // first explicit command or model turn.
    if !app.welcome_dismissed && app.turn_count == 0 && !app.running() {
        let welcome = Welcome {
            version: env!("CARGO_PKG_VERSION"),
            model: &app.cfg.model_name,
            workspace: &app.cfg.workspace_name,
            effort: app.reasoning_effort.as_deref(),
            branch: app.git_branch.as_deref(),
            recent: &app.recent_sessions,
        }
        .lines(frame.area().height as usize, height, transcript_width);
        frame.render_widget(Paragraph::new(Text::from(welcome)), transcript_area);
    } else {
        let max_scroll = projected_len.saturating_sub(height);
        stabilize_transcript_scroll(app, max_scroll);
        let end = projected_len.saturating_sub(app.scroll);
        let start = end.saturating_sub(height);
        let window: Vec<Line<'static>> = committed_transcript(app, !tail.is_empty())
            .iter()
            .chain(tail.iter())
            .skip(start)
            .take(end - start)
            .cloned()
            .collect();
        frame.render_widget(Paragraph::new(Text::from(window)), transcript_area);
    }

    if split_active {
        let inspected = selected_tool
            .and_then(|selected| app.activity_tools.get(selected))
            .or(app.split_snapshot.as_ref());
        let border_style = theme().dim;
        let inspector_width = split.content_width(inspector_area);
        // The pane's tabs lead its header in both tabs, so switching never
        // moves the strip.
        let agents = app.subagent_transcripts.len();
        let agents_label = if agents == 0 {
            "Agents".to_string()
        } else {
            format!("Agents {agents}")
        };
        let active_tab = match app.split_tab {
            SplitTab::Tools => 0,
            SplitTab::Agents => 1,
        };
        let mut tab_line = tab_strip(&["Tools", &agents_label], active_tab, "");
        tab_line.spans.insert(0, Span::raw("  "));
        tab_line.spans.push(Span::raw("   "));
        tab_line
            .spans
            .extend(keys::hint_spans(&[("tab", "switch")]));
        let tab_line = crate::tui::components::layout_fit(tab_line, inspector_width);
        let (header, body) = if app.split_tab == SplitTab::Agents {
            (
                vec![tab_line.clone(), Line::from("")],
                super::agents::split_agent_lines(app, inspector_width),
            )
        } else if let Some(tool) = inspected {
            let complete = tool.elapsed.is_some();
            let cache_valid = app.split_inspector_cache.as_ref().is_some_and(|cache| {
                cache.call_id == tool.call_id
                    && cache.complete == complete
                    && cache.has_output == tool.output.is_some()
                    && cache.is_error == tool.is_error
                    && cache.width == inspector_width
                    && cache.mode == app.inspector_mode
            });
            if !cache_valid {
                app.split_inspector_cache = Some(InspectorBodyCache {
                    call_id: tool.call_id.clone(),
                    complete,
                    has_output: tool.output.is_some(),
                    is_error: tool.is_error,
                    width: inspector_width,
                    mode: app.inspector_mode,
                    lines: tool_inspector_body_lines(tool, inspector_width, app.inspector_mode),
                });
            }
            (
                tool_inspector_header_lines(tool, inspector_width, app.inspector_mode),
                app.split_inspector_cache
                    .as_ref()
                    .map(|cache| cache.lines.clone())
                    .unwrap_or_default(),
            )
        } else {
            (empty_tool_inspector_lines(), Vec::new())
        };
        let header = if app.split_tab == SplitTab::Tools {
            let mut lines = vec![tab_line.clone(), Line::from("")];
            lines.extend(header);
            lines
        } else {
            header
        };
        let header_len = header.len();
        let [header_area, body_area] =
            Layout::vertical([Constraint::Length(header_len as u16), Constraint::Min(0)])
                .areas(inspector_area);
        let max_scroll = body
            .len()
            .saturating_sub(body_area.height as usize)
            .min(u16::MAX as usize) as u16;
        app.split_scroll = app.split_scroll.min(max_scroll);
        let divider = || split.block(border_style);
        frame.render_widget(
            Paragraph::new(Text::from(header)).block(divider()),
            header_area,
        );
        let body_start = app.split_scroll as usize;
        let body_end = (body_start + body_area.height as usize).min(body.len());
        frame.render_widget(
            Paragraph::new(Text::from(body[body_start..body_end].to_vec())).block(divider()),
            body_area,
        );
    }

    frame.render_widget(
        Paragraph::new(Text::from(live.window(live_height).to_vec())),
        live_area,
    );

    frame.render_widget(Paragraph::new(Text::from(composer.lines)), composer_area);
    frame.set_cursor_position((
        composer_area.x + composer.cursor_x,
        composer_area.y + composer.cursor_y,
    ));

    // Status line with contextual hints. Idle and running have live-region
    // indicators already; approval, questions, and a paused queue need the
    // persistent status label because they require user action.
    let g = glyphs();
    let state = if app.approval.is_some() {
        format!("{}awaiting approval", g.status_prefix(g.attention))
    } else if app.ask.is_some() {
        format!("{}awaiting answer", g.status_prefix(g.attention))
    } else if !app.prompt_queue.is_empty() {
        format!("{}queue paused", g.status_prefix(g.waiting))
    } else {
        String::new()
    };
    let approval_hint = app.approval.as_ref().map(|request| {
        ApprovalPrompt {
            tool_name: &request.tool_name,
            detail: &request.detail,
            yes_no: request.yes_no,
            selected: app.approval_choice,
        }
        .hint_pairs()
    });
    // What ctrl+t does next, named from where the layout is now.
    let view_hint = match app.view_mode {
        ViewMode::Classic => "split",
        ViewMode::Split => "main",
    };
    // Key hints as (key, action) pairs; an empty key is a plain note.
    let hint: Vec<(&str, &str)> = if let Some(hint) = approval_hint {
        hint
    } else if app.status_focus.is_some() {
        vec![("←→", "move"), ("enter", "open"), ("↑/esc", "composer")]
    } else if app.scroll > 0 {
        // Fresh scroll: name the two ways to get text out, since capture
        // means a plain drag will not select. Then it settles back to the
        // shorter form so the status line is not permanently crowded.
        if app.scroll_hint_live() && split_active {
            // A drag crosses both panes here, so name the key that does not.
            vec![
                ("", "drag spans panes"),
                ("/copy tool", "copies one"),
                ("pgdn", "to follow"),
            ]
        } else if app.scroll_hint_live() {
            vec![
                ("opt/shift+drag", "selects"),
                ("ctrl+y", "copies"),
                ("pgdn", "to follow"),
            ]
        } else {
            vec![("", "scrolled"), ("pgdn", "to follow")]
        }
    } else if app.ask.is_some() {
        vec![
            ("↑↓", "question"),
            ("←→", "option"),
            ("space", "choose"),
            ("tab", "next topic"),
            ("enter", "send"),
        ]
    } else if app.overlay.is_some() && app.approval.is_none() {
        if app.overlay_stack.is_empty() {
            vec![("↑↓", "move"), ("→/enter", "use"), ("esc", "close")]
        } else {
            vec![
                ("↑↓", "move"),
                ("←", "back"),
                ("→/enter", "use"),
                ("esc", "close"),
            ]
        }
    } else if app.palette_query().is_some() && app.approval.is_none() {
        vec![
            ("↑↓", "move"),
            ("→/enter", "use"),
            ("tab", "complete"),
            ("esc", "close"),
        ]
    } else if split_active && app.running() && app.activity_tools.is_empty() {
        vec![
            ("", "split ready"),
            ("", "waiting for tool call"),
            ("esc", "interrupt"),
        ]
    } else if app.running() {
        vec![("enter", "queue"), ("esc", "interrupt")]
    } else if !app.prompt_queue.is_empty() {
        vec![("enter", "resume"), ("/queue", "clear")]
    } else if app.last_answer.is_some() {
        // Only advertise copy once there is something to copy. The
        // status line has room for four hints; the composer's placeholder
        // already names @, so copy takes its place here.
        vec![
            ("enter", "send"),
            ("ctrl+y", "copy"),
            ("ctrl+o", "expand"),
            ("ctrl+t", view_hint),
        ]
    } else {
        vec![
            ("enter", "send"),
            ("@", "paths"),
            ("ctrl+o", "expand"),
            ("ctrl+t", view_hint),
        ]
    };
    let mut status = StatusBar::new();
    let context = if app.status_focus == Some(StatusFocus::Context) {
        Segment::new(
            context_segment(app.context_tokens, app.context_window, false),
            status_bar::CONTEXT,
        )
        .with_compact(context_segment(
            app.context_tokens,
            app.context_window,
            true,
        ))
        .with_style(theme().select)
    } else {
        Segment::spans(
            context_spans(app.context_tokens, app.context_window, false),
            status_bar::CONTEXT,
        )
        .with_compact_spans(context_spans(app.context_tokens, app.context_window, true))
    };
    status
        .push(Segment::new(
            format!("{}:{}", app.cfg.provider.label(), app.cfg.model_name),
            status_bar::MODEL,
        ))
        .push(Segment::new(
            app.reasoning_effort
                .as_deref()
                .map(str::to_string)
                .unwrap_or_default(),
            status_bar::EFFORT,
        ))
        .push(Segment::new(state, status_bar::KEEP))
        .push(Segment::new(
            if app.mcp_connecting {
                "MCP connecting · tools pending"
            } else {
                ""
            },
            status_bar::KEEP,
        ))
        .push(mode_label(&mode_segment(&app.cfg.mode, &app.cfg.plan)))
        .push(context);
    let active_agents = app
        .subagent_transcripts
        .values()
        .filter(|agent| agent.status.is_active())
        .count();
    let process_stat = g.counted(g.process, "procs", &app.cfg.stats.processes().to_string());
    for stat in stats_segments(&app.cfg.stats, active_agents) {
        let focus = if stat == process_stat {
            Some(StatusFocus::Processes)
        } else if stat.starts_with("agents ") {
            Some(StatusFocus::Agents)
        } else {
            None
        };
        let focused = focus == app.status_focus;
        let segment = Segment::new(
            stat,
            if focused {
                status_bar::KEEP
            } else {
                status_bar::STATS
            },
        );
        status.push(if focused {
            segment.with_style(theme().select)
        } else {
            segment
        });
    }
    status
        .push({
            let segment = Segment::new(todo_segment(&app.cfg.todos), status_bar::COUNTS);
            if app.status_focus == Some(StatusFocus::Todo) {
                segment.with_style(theme().select)
            } else {
                segment
            }
        })
        .push(Segment::new(
            queue_segment(app.prompt_queue.len()),
            status_bar::COUNTS,
        ))
        .push(hint_segment(&hint));
    let (workspace, workspace_compact) =
        workspace_segment(&app.cfg.workspace_name, app.git_branch.as_deref());
    status.trailing(Segment::new(workspace, status_bar::WORKSPACE).with_compact(workspace_compact));
    frame.render_widget(
        Paragraph::new(status.line(left_width, theme().dim)),
        status_area,
    );
}

pub(crate) fn transcript_content_width(app: &App, terminal_width: usize) -> usize {
    if SplitKind::side_by_side(app.view_mode, terminal_width) {
        terminal_width.saturating_mul(58) / 100
    } else {
        terminal_width
    }
}

pub(crate) fn stabilize_transcript_scroll(app: &mut App, max_scroll: usize) {
    if app.scroll > 0 {
        if max_scroll >= app.transcript_max_scroll {
            app.scroll = app
                .scroll
                .saturating_add(max_scroll - app.transcript_max_scroll);
        } else {
            app.scroll = app
                .scroll
                .saturating_sub(app.transcript_max_scroll - max_scroll);
        }
    }
    app.scroll = app.scroll.min(max_scroll);
    app.transcript_max_scroll = max_scroll;
}

/// The hint without its last ` · piece`: a tight row gives up the least
/// useful key before it gives up the model name.
#[cfg(test)]
pub(crate) fn shorter_hint(hint: &str) -> String {
    hint.rsplit_once(" · ")
        .map(|(head, _)| head)
        .unwrap_or(hint)
        .to_string()
}

/// The key-hint segment, bold keys and dim actions; its compact form
/// drops the last pair, the least useful key, before the bar drops
/// anything else.
pub(crate) fn hint_segment(hint: &[(&str, &str)]) -> Segment {
    let segment = Segment::spans(keys::hint_spans(hint), status_bar::HINT);
    match hint.split_last() {
        Some((_, head)) if !head.is_empty() => segment.with_compact_spans(keys::hint_spans(head)),
        _ => segment,
    }
}

/// A non-normal mode in its own colour, so each mode is known without
/// reading the row: plan, orchestrate, auto and yolo each take a distinct
/// bold colour from the theme, anything after the word stays dim. Text
/// only, no fill: a filled badge is heavier than the rest of the row.
/// Normal mode says nothing.
pub(crate) fn mode_label(mode: &str) -> Segment {
    let t = theme();
    if mode.is_empty() {
        return Segment::new("", status_bar::KEEP);
    }
    let (word, rest) = mode.split_once(' ').unwrap_or((mode, ""));
    let style = match word {
        "plan" => t.modes.plan,
        "orchestrate" => t.modes.orchestrate,
        "auto" => t.modes.auto,
        "yolo" => t.modes.yolo,
        _ => t.strong,
    };
    let mut spans = vec![Span::styled(word.to_string(), style)];
    if !rest.is_empty() {
        spans.push(Span::styled(format!(" {rest}"), t.dim));
    }
    Segment::spans(spans, status_bar::KEEP)
}
