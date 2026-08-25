// Rendering: builds the terminal line models — the welcome screen, the
// transcript/tool rails, the live region (queue/working/spinner), status
// segments, and every picker overlay. Pure layout: reads `App` state and
// returns `Vec<Line>`; the terminal loop in the parent `run`/`draw`
// drives the actual frames.

use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::tui::components::composer::Composer;
use crate::tui::components::status_bar::StatusBar;
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
use super::transcript::*;

/// Breathing room between inspector content and the terminal edge. The
/// renderer asks the padded block for its inner width, so previews wrap to
/// the real content box rather than compensating with scattered subtraction.
const INSPECTOR_PADDING: Padding = Padding::right(1);

fn inspector_block(border_style: ratatui::style::Style) -> Block<'static> {
    Block::default()
        .borders(Borders::LEFT)
        .border_style(border_style)
        .padding(INSPECTOR_PADDING)
}

fn inspector_content_width(area: ratatui::layout::Rect) -> usize {
    inspector_block(ratatui::style::Style::default())
        .inner(area)
        .width as usize
}

pub(crate) fn draw(frame: &mut Frame, app: &mut App) {
    let width = frame.area().width as usize;
    // Split the whole terminal first so the transcript, live rail, composer,
    // and status share one left column and the inspector owns the full right.
    let split_active = app.view_mode == ViewMode::Split && width >= 100;
    let [left_root, inspector_area] = if split_active {
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
            .areas(frame.area())
    } else {
        [frame.area(), frame.area()]
    };
    let left_width = left_root.width as usize;
    let live = live_lines(app, left_width);
    // Let a todo rail use the available height rather than silently
    // clipping later steps. Preserve three transcript rows plus the gap,
    // composer, and status rows on short terminals.
    let live_height = live
        .len()
        .min((left_root.height as usize).saturating_sub(6)) as u16;
    let [transcript_area, live_area, _composer_gap_area, composer_area, status_area] =
        Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(live_height),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(left_root);

    // Transcript: committed history plus a render-only projection of the
    // in-progress turn. Deltas therefore appear in their final location
    // instead of streaming through the temporary area and jumping here.
    let height = transcript_area.height as usize;
    let transcript_width = transcript_area.width as usize;
    let selected_tool = (split_active && !app.activity_tools.is_empty()).then(|| {
        app.split_tool
            .unwrap_or_else(|| app.activity_tools.len().saturating_sub(1))
            .min(app.activity_tools.len().saturating_sub(1))
    });
    let projected = if selected_tool.is_some() {
        projected_transcript_selected(app, transcript_width, selected_tool)
    } else {
        projected_transcript(app, transcript_width)
    };
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
        let max_scroll = projected.len().saturating_sub(height);
        stabilize_transcript_scroll(app, max_scroll);
        let end = projected.len().saturating_sub(app.scroll);
        let start = end.saturating_sub(height);
        frame.render_widget(
            Paragraph::new(Text::from(projected[start..end].to_vec())),
            transcript_area,
        );
    }

    if split_active {
        let inspected = selected_tool
            .and_then(|selected| app.activity_tools.get(selected))
            .or(app.split_snapshot.as_ref());
        let border_style = theme().dim;
        let inspector_width = inspector_content_width(inspector_area);
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
        let divider = || inspector_block(border_style);
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

    frame.render_widget(Paragraph::new(Text::from(live)), live_area);

    let placeholder = if app.ask.is_some() {
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
        composer_area.width as usize,
        theme().accent,
        theme().dim,
        &pill_spans,
    )
    .render();
    frame.render_widget(Paragraph::new(composer.line), composer_area);
    frame.set_cursor_position((composer_area.x + composer.cursor_x, composer_area.y));

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
    let hint = if app.scroll > 0 {
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
        "↑↓ navigate · enter use · esc close"
    } else if app.palette_query().is_some() && app.approval.is_none() {
        "↑↓ navigate · enter use · tab complete · esc close"
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
    let mode = mode_segment(&app.cfg.mode, &app.cfg.plan);
    let context = context_segment(app.context_tokens, app.context_window);
    let stats = stats_segments(&app.cfg.stats);
    let todo = todo_segment(&app.cfg.todos);
    let queue = queue_segment(app.prompt_queue.len());
    let workspace = workspace_status_name(&app.cfg.workspace_name);
    let status = StatusBar {
        model: &app.cfg.model_name,
        state,
        mode: &mode,
        context: &context,
        stats: &stats,
        todo: &todo,
        queue: &queue,
        hint,
        workspace,
    };
    frame.render_widget(
        Paragraph::new(status.line(left_width, theme().dim)),
        status_area,
    );
}

pub(crate) fn transcript_content_width(app: &App, terminal_width: usize) -> usize {
    if app.view_mode == ViewMode::Split && terminal_width >= 100 {
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
pub(crate) fn stats_segments(stats: &orca_harness_tools::BackgroundStats) -> String {
    let mut out = String::new();
    if stats.processes() > 0 {
        out.push_str(&format!(" · procs {}", stats.processes()));
    }
    if stats.kernels() > 0 {
        out.push_str(" · pykernel");
    }
    if stats.bun_repls() > 0 {
        out.push_str(" · bun_repl");
    }
    if stats.agents() > 0 {
        out.push_str(&format!(" · agents {}", stats.agents()));
    }
    out
}

pub(crate) fn queue_segment(queued: usize) -> String {
    if queued == 0 {
        String::new()
    } else {
        format!(" · queued {queued}")
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
        crate::mode::Mode::Yolo => return " · yolo".to_string(),
        crate::mode::Mode::Plan => {}
    }
    match plan.written().len() {
        0 => " · plan mode".to_string(),
        1 => " · plan mode · 1 plan".to_string(),
        n => format!(" · plan mode · {n} plans"),
    }
}

/// Progress through the agent's task list, once it has one.
pub(crate) fn todo_segment(todos: &orca_harness_tools::TodoList) -> String {
    match todos.progress() {
        (_, 0) => String::new(),
        (done, total) => format!(" · todo {done}/{total}"),
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
        let last = index + 1 == visible && overflow == 0;
        let branch = if last { "└" } else { "├" };
        let label = if index == 0 {
            "next".to_string()
        } else {
            (index + 1).to_string()
        };
        let available = width.saturating_sub(12).max(8);
        lines.push(Line::from(vec![
            Span::styled(format!("  {branch} "), t.dim),
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
        lines.push(Line::from(Span::styled(
            format!("  └      +{overflow} more"),
            t.dim,
        )));
    }
    lines
}

/// The pinned live region: approval prompt beats ask form, overlays, palette,
/// then run status. Streaming content itself is projected into the main transcript.
pub(crate) fn live_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    if let Some(request) = &app.approval {
        return vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  approval required: {}", request.tool_name),
                t.warn,
            )),
            Line::from(Span::raw(format!(
                "    {}",
                view::truncate_line(&request.detail, width.saturating_sub(6))
            ))),
            Line::from(Span::styled(
                "    [y] allow once   [a] always (this session)   [A] always (save for workspace)   [n] deny",
                t.dim,
            )),
        ];
    }
    if let Some(ask) = &app.ask {
        return ask.lines(width);
    }
    if let Some(overlay) = &app.overlay {
        return match overlay {
            Overlay::Help { filter, picker } => help_picker_lines(filter, picker, width),
            Overlay::Models(picker) => model_picker_lines(picker, PICKER_ROWS + 2, width),
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
            Overlay::Approvals { tools, picker } => approvals_lines(tools, picker, width),
            Overlay::Extensions { picker } => extensions_picker_lines(picker, width),
            Overlay::Mcp {
                servers,
                filter,
                picker,
            } => mcp_picker_lines(servers, &app.cfg.mcp, filter, picker, width),
            Overlay::Skills {
                entries,
                filter,
                picker,
            } => skills_picker_lines(entries, filter, picker, width),
            Overlay::Sessions { sessions, picker } => {
                sessions_picker_lines(sessions, app.cfg.session_id.as_deref(), picker, width)
            }
        };
    }
    if app.palette_query().is_some() {
        return palette_lines(app, PALETTE_ROWS + 2, width);
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
        return lines;
    }
    let mut lines = queue_lines(app, width);
    lines.extend(todo_lines(&app.cfg.todos, width));
    if let Some(summary) = &app.last_turn_summary {
        lines.push(Line::from(Span::styled(format!("  {summary}"), t.dim)));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspector_content_width_comes_from_the_padded_block() {
        let area = ratatui::layout::Rect::new(0, 0, 50, 20);
        let block = inspector_block(ratatui::style::Style::default());
        assert_eq!(
            inspector_content_width(area),
            block.inner(area).width as usize
        );
        assert_eq!(block.inner(area).right(), area.right() - 1);
    }
}
