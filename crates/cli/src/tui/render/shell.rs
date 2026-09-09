// Rendering: builds the terminal line models — the welcome screen, the
// transcript/tool rails, the live region (queue/working/spinner), status
// segments, and every picker overlay. Pure layout: reads `App` state and
// returns `Vec<Line>`; the terminal loop in the parent `run`/`draw`
// drives the actual frames.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::tui::components::approval::ApprovalPrompt;
use crate::tui::components::composer::Composer;
use crate::tui::components::status_bar::{self, Segment, StatusBar};
use crate::tui::components::welcome::Welcome;
use crate::view::glyphs::glyphs;
use crate::view::theme;

use super::super::inspector::{
    empty_tool_inspector_lines, tool_inspector_body_lines, tool_inspector_header_lines,
};
use super::super::{App, InspectorBodyCache, StatusFocus, ViewMode};
use super::agents::draw_agent_browser;
use super::overlays::context_segment;
use super::transcript::*;

mod live;
pub(crate) use live::*;

/// Breathing room between inspector content and the terminal edge. The
/// renderer asks the padded block for its inner width, so previews wrap to
/// the real content box rather than compensating with scattered subtraction.
const INSPECTOR_PADDING: Padding = Padding::right(1);
/// Narrowest terminal that fits the inspector beside the transcript.
pub(crate) const SPLIT_MIN_WIDTH: usize = 100;
/// Shortest terminal that fits the inspector under the transcript.
const STACK_MIN_HEIGHT: usize = 18;

/// Where the inspector goes in split view. Geometry is decided here once;
/// the transcript wrap width and the wheel routing both follow it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SplitKind {
    Off,
    /// Inspector on the right, 42% of the width.
    SideBySide,
    /// Too narrow for a second column: inspector under the transcript.
    Stacked,
}

impl SplitKind {
    pub(crate) fn side_by_side(mode: ViewMode, width: usize) -> bool {
        mode == ViewMode::Split && width >= SPLIT_MIN_WIDTH
    }

    pub(crate) fn for_area(mode: ViewMode, width: usize, height: usize) -> Self {
        if Self::side_by_side(mode, width) {
            Self::SideBySide
        } else if mode == ViewMode::Split && height >= STACK_MIN_HEIGHT {
            Self::Stacked
        } else {
            Self::Off
        }
    }

    /// `[conversation, inspector]`; both are the whole area when off.
    fn areas(self, area: Rect) -> [Rect; 2] {
        match self {
            Self::SideBySide => self.areas_at(area, 58),
            Self::Stacked => self.areas_at(area, 60),
            Self::Off => [area, area],
        }
    }

    /// `[first, second]` with `first_percent` of the width (side by side)
    /// or the height (stacked) going to the first pane.
    pub(crate) fn areas_at(self, area: Rect, first_percent: u16) -> [Rect; 2] {
        let constraints = [
            Constraint::Percentage(first_percent),
            Constraint::Percentage(100 - first_percent),
        ];
        match self {
            Self::SideBySide => Layout::horizontal(constraints).areas(area),
            Self::Stacked => Layout::vertical(constraints).areas(area),
            Self::Off => [area, area],
        }
    }

    /// The second pane's chrome: a divider on the side it shares with the
    /// first pane and a padded outer edge.
    pub(crate) fn block(self, border_style: ratatui::style::Style) -> Block<'static> {
        let borders = match self {
            Self::Stacked => Borders::TOP,
            Self::SideBySide | Self::Off => Borders::LEFT,
        };
        Block::default()
            .borders(borders)
            .border_style(border_style)
            .padding(INSPECTOR_PADDING)
    }

    pub(crate) fn content_width(self, area: Rect) -> usize {
        self.block(ratatui::style::Style::default())
            .inner(area)
            .width as usize
    }
}

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
    let composer = Composer::new(
        &app.composer,
        app.cursor,
        placeholder,
        left_width,
        theme().accent,
        theme().dim,
        &pill_spans,
    )
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
        let (header, body) = if let Some(tool) = inspected {
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
        }
        .hint()
    });
    let hint = if let Some(hint) = approval_hint.as_deref() {
        hint
    } else if app.status_focus.is_some() {
        "←→ move · enter open · ↑/esc composer"
    } else if app.scroll > 0 {
        // Fresh scroll: name the two ways to get text out, since capture
        // means a plain drag will not select. Then it settles back to the
        // shorter form so the status line is not permanently crowded.
        if app.scroll_hint_live() && split_active {
            // A drag crosses both panes here, so name the key that does not.
            "drag spans panes · /copy tool copies one · pgdn to follow"
        } else if app.scroll_hint_live() {
            "opt/shift+drag selects · ctrl+y copies · pgdn to follow"
        } else {
            "scrolled · pgdn to follow"
        }
    } else if app.ask.is_some() {
        "↑↓ question · ←→ option · space choose · tab topic · enter send"
    } else if app.overlay.is_some() && app.approval.is_none() {
        if app.overlay_stack.is_empty() {
            "↑↓ move · →/enter use · esc close"
        } else {
            "↑↓ move · ← back · →/enter use · esc close"
        }
    } else if app.palette_query().is_some() && app.approval.is_none() {
        "↑↓ move · →/enter use · tab complete · esc close"
    } else if split_active && app.running() && app.activity_tools.is_empty() {
        "split ready · waiting for tool call · esc interrupt"
    } else if app.running() {
        "enter queue · esc interrupt"
    } else if !app.prompt_queue.is_empty() {
        "enter resume · /queue clear"
    } else if app.last_answer.is_some() {
        // Only advertise copy once there is something to copy. The
        // status line has room for four hints, and before the first
        // answer lands scrolling is the more useful thing to name.
        "enter send · @ paths · ctrl+y copy · ctrl+o expand"
    } else {
        "enter send · @ paths · ctrl+o expand · wheel scroll"
    };
    let mut status = StatusBar::new();
    let context = Segment::new(
        context_segment(app.context_tokens, app.context_window, false),
        status_bar::CONTEXT,
    )
    .with_compact(context_segment(
        app.context_tokens,
        app.context_window,
        true,
    ));
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
            mode_segment(&app.cfg.mode, &app.cfg.plan),
            status_bar::KEEP,
        ))
        .push(if app.status_focus == Some(StatusFocus::Context) {
            context.with_style(theme().select)
        } else {
            context
        });
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
        .push(Segment::new(hint, status_bar::HINT).with_compact(shorter_hint(hint)));
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
pub(crate) fn shorter_hint(hint: &str) -> String {
    hint.rsplit_once(" · ")
        .map(|(head, _)| head)
        .unwrap_or(hint)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shorter_hint_drops_its_last_piece_only() {
        assert_eq!(shorter_hint("a · b · c"), "a · b");
        assert_eq!(shorter_hint("alone"), "alone");
    }

    #[test]
    fn inspector_content_width_comes_from_the_padded_block() {
        let area = Rect::new(0, 0, 50, 20);
        let block = SplitKind::SideBySide.block(ratatui::style::Style::default());
        assert_eq!(
            SplitKind::SideBySide.content_width(area),
            block.inner(area).width as usize
        );
        assert_eq!(block.inner(area).right(), area.right() - 1);
    }

    #[test]
    fn split_falls_back_to_stacking_on_narrow_terminals() {
        assert_eq!(
            SplitKind::for_area(ViewMode::Split, 120, 24),
            SplitKind::SideBySide
        );
        assert_eq!(
            SplitKind::for_area(ViewMode::Split, 90, 30),
            SplitKind::Stacked
        );
        assert_eq!(SplitKind::for_area(ViewMode::Split, 90, 12), SplitKind::Off);
        assert_eq!(
            SplitKind::for_area(ViewMode::Classic, 200, 60),
            SplitKind::Off
        );
        let [top, bottom] = SplitKind::Stacked.areas(Rect::new(0, 0, 90, 30));
        assert_eq!(top.width, 90);
        assert_eq!(top.height + bottom.height, 30);
    }
}
