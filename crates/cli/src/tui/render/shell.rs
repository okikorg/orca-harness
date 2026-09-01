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
use crate::tui::components::tree::TreeBranch;
use crate::tui::components::welcome::Welcome;
use crate::view::{self, theme};

use super::super::format::{elapsed_label, fmt_turn_tokens, workspace_status_name};
use super::super::inspector::{
    empty_tool_inspector_lines, tool_inspector_body_lines, tool_inspector_header_lines,
};
use super::super::{
    App, InspectorBodyCache, Overlay, RunState, ViewMode, PALETTE_ROWS, PICKER_ROWS,
    QUEUE_PREVIEW_ROWS, SPINNER,
};
use super::overlays::*;
use super::pickers::*;
use super::plugins::*;
use super::transcript::*;

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
            Self::SideBySide => {
                Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                    .areas(area)
            }
            Self::Stacked => {
                Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .areas(area)
            }
            Self::Off => [area, area],
        }
    }

    fn block(self, border_style: ratatui::style::Style) -> Block<'static> {
        let borders = match self {
            Self::Stacked => Borders::TOP,
            Self::SideBySide | Self::Off => Borders::LEFT,
        };
        Block::default()
            .borders(borders)
            .border_style(border_style)
            .padding(INSPECTOR_PADDING)
    }

    fn content_width(self, area: Rect) -> usize {
        self.block(ratatui::style::Style::default())
            .inner(area)
            .width as usize
    }
}

/// Rows the layout keeps for the conversation around the live region:
/// three transcript rows, the composer gap and the status line.
const RESERVED_ROWS: usize = 5;

pub(crate) fn draw(frame: &mut Frame, app: &mut App) {
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
        let full_height = frame.area().height as usize;
        let welcome = Welcome {
            version: env!("CARGO_PKG_VERSION"),
            model: &app.cfg.model_name,
            workspace: &app.cfg.workspace_name,
        }
        .lines(full_height, height, transcript_width);
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

    // Status line with contextual hints.
    let state = if app.approval.is_some() {
        "awaiting approval"
    } else if app.ask.is_some() {
        "awaiting answer"
    } else if app.running() {
        "running"
    } else if !app.prompt_queue.is_empty() {
        "queue paused"
    } else {
        "idle"
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
    status
        .push(Segment::new(&app.cfg.model_name, status_bar::MODEL))
        .push(Segment::new(
            app.reasoning_effort
                .as_deref()
                .map(|effort| format!("effort {effort}"))
                .unwrap_or_default(),
            status_bar::EFFORT,
        ))
        .push(Segment::new(state, status_bar::KEEP))
        .push(Segment::new(
            mode_segment(&app.cfg.mode, &app.cfg.plan),
            status_bar::KEEP,
        ))
        .push(Segment::new(
            context_segment(app.context_tokens, app.context_window),
            status_bar::CONTEXT,
        ));
    for stat in stats_segments(&app.cfg.stats) {
        status.push(Segment::new(stat, status_bar::STATS));
    }
    status
        .push(Segment::new(
            todo_segment(&app.cfg.todos),
            status_bar::COUNTS,
        ))
        .push(Segment::new(
            queue_segment(app.prompt_queue.len()),
            status_bar::COUNTS,
        ))
        .push(Segment::new(hint, status_bar::HINT))
        .trailing(Segment::new(
            workspace_status_name(&app.cfg.workspace_name),
            status_bar::WORKSPACE,
        ));
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

/// Status-line segments for live background work; empty when idle so the
/// line stays quiet. Each persistent compute tool is unnumbered (0 or 1).
pub(crate) fn stats_segments(stats: &orca_harness_tools::BackgroundStats) -> Vec<String> {
    let mut out = Vec::new();
    if stats.processes() > 0 {
        out.push(format!("procs {}", stats.processes()));
    }
    if stats.kernels() > 0 {
        out.push("pykernel".to_string());
    }
    if stats.bun_repls() > 0 {
        out.push("bun_repl".to_string());
    }
    if stats.agents() > 0 {
        out.push(format!("agents {}", stats.agents()));
    }
    out
}

pub(crate) fn queue_segment(queued: usize) -> String {
    if queued == 0 {
        String::new()
    } else {
        format!("queued {queued}")
    }
}

/// Plan mode is a restriction the user cannot be allowed to forget: it
/// sits next to the run state, not among the optional segments, and it
/// is the one segment that never abbreviates away. Normal mode says
/// nothing — the absence of the word is the normal case.
///
/// Once the agent has written a plan, the count rides along, so a landed
/// plan is visible without waiting for `/mode normal` to list it.
///
/// Yolo is the same bargain from the other side: it silences exactly
/// the mechanism whose job is to say "wait", so while it is on its
/// segment never abbreviates away either. It renders as plain `yolo`.
pub(crate) fn mode_segment(mode: &crate::mode::ModeHandle, plan: &crate::plan::PlanArea) -> String {
    match mode.get() {
        crate::mode::Mode::Normal => {
            return String::new();
        }
        crate::mode::Mode::Auto => return "auto".to_string(),
        crate::mode::Mode::Yolo => return "yolo".to_string(),
        crate::mode::Mode::Plan => {}
    }
    match plan.written().len() {
        0 => "plan mode".to_string(),
        1 => "plan mode · 1 plan".to_string(),
        n => format!("plan mode · {n} plans"),
    }
}

/// Progress through the agent's task list, once it has one.
pub(crate) fn todo_segment(todos: &orca_harness_tools::TodoList) -> String {
    match todos.progress() {
        (_, 0) => String::new(),
        (done, total) => format!("todo {done}/{total}"),
    }
}

/// A compact execution rail. Prompts stay out of the transcript until
/// they start, so the conversation preserves its actual chronology.
pub(crate) fn queue_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    if app.prompt_queue.is_empty() {
        return Vec::new();
    }

    let mut lines = vec![Line::from(vec![
        Span::styled("  queued", t.strong),
        Span::styled(format!(" · {}", app.prompt_queue.len()), t.dim),
    ])];
    let visible = app.prompt_queue.len().min(QUEUE_PREVIEW_ROWS);
    let overflow = app.prompt_queue.len().saturating_sub(visible);
    for (index, prompt) in app.prompt_queue.iter().take(visible).enumerate() {
        let branch = TreeBranch {
            indent: "  ",
            last: index + 1 == visible && overflow == 0,
        };
        let label = if index == 0 {
            "next".to_string()
        } else {
            (index + 1).to_string()
        };
        let available = width.saturating_sub(12).max(8);
        lines.push(Line::from(vec![
            Span::styled(branch.prefix(), t.dim),
            Span::styled(
                format!("{label:<4} "),
                if index == 0 { t.accent } else { t.dim },
            ),
            Span::styled(
                view::truncate_line(prompt, available),
                if index == 0 { t.strong } else { t.dim },
            ),
        ]));
    }
    if overflow > 0 {
        let branch = TreeBranch {
            indent: "  ",
            last: true,
        };
        lines.push(Line::from(Span::styled(
            format!("{}     +{overflow} more", branch.prefix()),
            t.dim,
        )));
    }
    lines
}

/// Which end of the live region survives when it is taller than the space.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LiveAnchor {
    /// Prompts, overlays and pickers are read from their header down.
    Top,
    /// While a run is in progress the spinner row at the foot is what the
    /// user watches, so a tall queue or todo list gives up its head.
    Bottom,
}

pub(crate) struct LiveRegion {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) anchor: LiveAnchor,
}

impl LiveRegion {
    fn top(lines: Vec<Line<'static>>) -> Self {
        Self {
            lines,
            anchor: LiveAnchor::Top,
        }
    }

    /// The rows that fit in `height`, taken from the anchored end.
    pub(crate) fn window(&self, height: usize) -> &[Line<'static>] {
        let len = self.lines.len();
        match self.anchor {
            LiveAnchor::Top => &self.lines[..height.min(len)],
            LiveAnchor::Bottom => &self.lines[len.saturating_sub(height)..],
        }
    }
}

/// The pinned live region's rows; see [`live_region`] for the anchoring.
pub(crate) fn live_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    live_region(app, width).lines
}

/// The pinned live region: approval prompt beats ask form, overlays, palette,
/// then run status. Streaming content itself is projected into the main transcript.
pub(crate) fn live_region(app: &App, width: usize) -> LiveRegion {
    let t = theme();
    if let Some(request) = &app.approval {
        return LiveRegion::top(
            ApprovalPrompt {
                tool_name: &request.tool_name,
                detail: &request.detail,
                yes_no: request.yes_no,
            }
            .lines(width),
        );
    }
    if let Some(ask) = &app.ask {
        return LiveRegion::top(ask.lines(width));
    }
    if let Some(overlay) = &app.overlay {
        let mut lines = match overlay {
            Overlay::Help { filter, picker } => help_picker_lines(filter, picker, width),
            Overlay::Models(picker) => model_picker_lines(picker, PICKER_ROWS + 2, width),
            Overlay::Efforts(picker) => effort_picker_lines(picker, width),
            Overlay::Locations(picker) => location_picker_lines(picker, width),
            Overlay::SkillMentions(picker) => skill_mention_picker_lines(picker, width),
            Overlay::Providers { picker } => provider_lines(picker, width),
            Overlay::Themes { picker } => theme_picker_lines(picker, width),
            Overlay::Views { picker } => view_picker_lines(app.view_mode, picker, width),
            Overlay::Mode { picker } => mode_picker_lines(app.cfg.mode.get(), picker, width),
            Overlay::TranscriptSpacing { picker } => transcript_spacing_lines(picker, width),
            Overlay::Inspector { picker } => {
                inspector_picker_lines(app.inspector_mode, picker, width)
            }
            Overlay::Usage => usage_lines(app, width),
            Overlay::ApiKey { provider, input } => api_key_lines(*provider, input),
            Overlay::Settings { picker } => settings_lines(app, picker, width),
            Overlay::Subagents { picker } => subagent_settings_lines(app, picker, width),
            Overlay::SubagentValues {
                setting,
                values,
                picker,
            } => subagent_value_lines(*setting, values, picker, width),
            Overlay::Approvals { tools, picker } => approvals_lines(tools, picker, width),
            Overlay::Extensions { picker } => extensions_picker_lines(picker, width),
            Overlay::Mcp {
                entries,
                filter,
                picker,
            } => crate::tui::mcp_picker::lines(entries, &app.cfg.mcp, filter, picker, width),
            Overlay::Plugins {
                entries,
                filter,
                picker,
            } => plugin_picker_lines(
                entries,
                &app.cfg.mcp,
                &app.cfg.skills,
                filter,
                picker,
                width,
            ),
            Overlay::Skills {
                entries,
                filter,
                picker,
            } => crate::tui::skills_picker::lines(entries, filter, picker, width),
            Overlay::Sessions { sessions, picker } => {
                sessions_picker_lines(sessions, app.cfg.session_id.as_deref(), picker, width)
            }
        };
        if !app.overlay_stack.is_empty() {
            if let Some(span) = lines.first_mut().and_then(|line| line.spans.first_mut()) {
                span.content = span
                    .content
                    .replace(" · esc close", " · ← back · esc close")
                    .into();
            }
        }
        return LiveRegion::top(lines);
    }
    if app.palette_query().is_some() {
        return LiveRegion::top(palette_lines(app, PALETTE_ROWS + 2, width));
    }
    if app.running() {
        let mut lines = queue_lines(app, width);
        lines.extend(todo_lines(&app.cfg.todos, width));
        let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
        let verb = if !app.text.is_empty() || app.pending_assistant.is_some() {
            "writing"
        } else if !app.reasoning.is_empty() {
            "thinking"
        } else {
            "working"
        };
        if let RunState::Running { started, .. } = &app.run {
            let token_io = if app.turn_tokens_in == 0 && app.turn_tokens_out == 0 {
                String::new()
            } else {
                format!(
                    " · ↑{} ↓{}",
                    fmt_turn_tokens(app.turn_tokens_in),
                    fmt_turn_tokens(app.turn_tokens_out)
                )
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {spinner} "), t.accent),
                Span::styled(
                    format!(
                        "{verb} · {}{token_io} · esc to interrupt",
                        elapsed_label(started.elapsed())
                    ),
                    t.dim,
                ),
            ]));
        }
        return LiveRegion {
            lines,
            anchor: LiveAnchor::Bottom,
        };
    }
    let mut lines = queue_lines(app, width);
    lines.extend(todo_lines(&app.cfg.todos, width));
    if let Some(summary) = &app.last_turn_summary {
        lines.push(Line::from(Span::styled(format!("  {summary}"), t.dim)));
    }
    LiveRegion {
        lines,
        anchor: LiveAnchor::Bottom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_running_live_region_keeps_its_foot() {
        let region = LiveRegion {
            lines: (0..5).map(|n| Line::from(n.to_string())).collect(),
            anchor: LiveAnchor::Bottom,
        };
        let shown: Vec<String> = region
            .window(2)
            .iter()
            .map(|line| line.to_string())
            .collect();
        assert_eq!(shown, ["3", "4"]);
        let region = LiveRegion::top(region.lines);
        assert_eq!(region.window(2).len(), 2);
        assert_eq!(region.window(2)[0].to_string(), "0");
        assert_eq!(region.window(9).len(), 5);
    }
}
