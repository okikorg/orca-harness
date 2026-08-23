//! Rendering: builds the terminal line models — the welcome screen, the
//! transcript/tool rails, the live region (queue/working/spinner), status
//! segments, and every picker overlay. Pure layout: reads `App` state and
//! returns `Vec<Line>`; the terminal loop in the parent `run`/`draw`
//! drives the actual frames.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use orca_harness_core::Message;
use orca_harness_tools::TodoStatus;

use crate::commands::filter_commands;
use crate::components::picker::ListPicker;
use crate::msg::Provider;
use crate::view::{self, theme};

use super::format::{
    age_label, byte_index, elapsed_label, fmt_tokens, plural, redact_command, size,
    workspace_status_name,
};
use super::inspector::{
    empty_tool_inspector_lines, tool_inspector_body_lines, tool_inspector_header_lines,
};
use super::{
    append_render_block, App, BlockSpacing, InspectorBodyCache, LocationEntry, LocationPicker,
    ModelPicker, Overlay, RunState, ToolActivity, TuiConfig, ViewMode, LIVE_TOOL_ROWS,
    PALETTE_ROWS, PICKER_ROWS, QUEUE_PREVIEW_ROWS, SESSIONS_WINDOW, SPINNER,
};

fn welcome_lines(
    full_height: usize,
    clip: usize,
    width: usize,
    cfg: &TuiConfig,
) -> Vec<Line<'static>> {
    let t = theme();
    let available_width = width.saturating_sub(4).min(64);
    let value_width = available_width.saturating_sub(11);
    let row = |label: &'static str, value: &str| {
        Line::from(vec![
            Span::styled(format!("{label:<11}"), t.dim),
            Span::raw(view::truncate_line(value, value_width)),
        ])
    };
    let content = vec![
        Line::from(vec![
            Span::styled("▀▄ ", t.accent),
            Span::styled("ORCACODE", t.strong),
            Span::styled(format!("  v{}", env!("CARGO_PKG_VERSION")), t.dim),
        ]),
        Line::from(Span::styled(
            "A small, fast agent runtime for your terminal",
            t.dim,
        )),
        row("model", &cfg.model_name),
        row("workspace", &cfg.workspace_name),
        Line::from(vec![
            Span::styled("› ", t.accent),
            Span::styled("Describe a task to begin", t.strong),
        ]),
        Line::from(Span::styled(
            "  /help commands · /models switch model",
            t.dim,
        )),
    ];
    // Centre the pixels the user can actually see. Previously this used
    // the 64-column maximum even when the longest rendered row was much
    // shorter, leaving the visible card noticeably left of centre.
    let content_width = content.iter().map(Line::width).max().unwrap_or(0);
    let indent = " ".repeat(width.saturating_sub(content_width) / 2);
    let content = content.into_iter().map(|line| {
        let mut spans = Vec::with_capacity(line.spans.len() + 1);
        spans.push(Span::raw(indent.clone()));
        spans.extend(line.spans);
        Line::from(spans)
    });
    // Centre against the full terminal height, but never push the bottom
    // of the card past the transcript clip the welcome is drawn into.
    let top =
        (full_height.saturating_sub(content.len()) / 2).min(clip.saturating_sub(content.len()));
    std::iter::repeat_n(Line::from(""), top)
        .chain(content)
        .collect()
}

pub(super) fn draw(frame: &mut Frame, app: &mut App) {
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
    // first real turn.
    if app.turn_count == 0 && !app.running() {
        let full_height = frame.area().height as usize;
        let welcome = welcome_lines(full_height, height, transcript_width, &app.cfg);
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
        let inspector_width = inspector_area.width as usize;
        let (header, body) = if let Some(tool) = inspected {
            let complete = tool.output.is_some();
            let cache_valid = app.split_inspector_cache.as_ref().is_some_and(|cache| {
                cache.call_id == tool.call_id
                    && cache.complete == complete
                    && cache.is_error == tool.is_error
                    && cache.width == inspector_width
            });
            if !cache_valid {
                app.split_inspector_cache = Some(InspectorBodyCache {
                    call_id: tool.call_id.clone(),
                    complete,
                    is_error: tool.is_error,
                    width: inspector_width,
                    lines: tool_inspector_body_lines(tool, inspector_width),
                });
            }
            (
                tool_inspector_header_lines(tool, inspector_width),
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
        let border_style = if app.split_focused {
            theme().accent
        } else {
            theme().dim
        };
        let divider = || {
            Block::default()
                .borders(Borders::LEFT)
                .border_style(border_style)
        };
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

    // Composer with a horizontally-scrolling single line and a
    // placeholder when empty.
    let inner_width = (composer_area.width as usize).saturating_sub(3).max(8);
    let chars: Vec<char> = app.composer.chars().collect();
    let start = if app.cursor >= inner_width {
        app.cursor + 1 - inner_width
    } else {
        0
    };
    let visible: String = chars.iter().skip(start).take(inner_width).collect();
    let composer_line = if app.composer.is_empty() {
        let placeholder = if app.running() {
            "type another prompt to queue"
        } else if !app.prompt_queue.is_empty() {
            "queue paused · enter to resume"
        } else {
            "ask anything · @ add files · /help commands"
        };
        Line::from(vec![
            Span::styled("│ ", theme().accent),
            Span::styled(placeholder, theme().dim),
        ])
    } else {
        Line::from(vec![Span::styled("│ ", theme().accent), Span::raw(visible)])
    };
    frame.render_widget(Paragraph::new(composer_line), composer_area);
    frame.set_cursor_position((
        composer_area.x + 2 + (app.cursor - start) as u16,
        composer_area.y,
    ));

    // Status line with contextual hints.
    let state = if app.approval.is_some() {
        "awaiting approval"
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
            "drag spans panes · ctrl+y copies one · pgdn to follow"
        } else if app.scroll_hint_live() {
            "opt/shift+drag selects · ctrl+y copies · pgdn to follow"
        } else {
            "scrolled · pgdn to follow"
        }
    } else if app.overlay.is_some() && app.approval.is_none() {
        "↑↓ navigate · enter use · esc close"
    } else if app.palette_query().is_some() && app.approval.is_none() {
        "↑↓ navigate · enter use · tab complete · esc close"
    } else if app.split_focused {
        "↑↓ select tool · ctrl+y copy tool · tab return"
    } else if split_active && !app.activity_tools.is_empty() {
        "tab inspect · enter queue · esc interrupt"
    } else if split_active && app.running() {
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
    let status = format!(
        " {} · {}{} · {}{}{}{} · {} · {}",
        app.cfg.model_name,
        state,
        mode_segment(&app.cfg.mode, &app.cfg.plan),
        context_segment(app.context_tokens, app.context_window),
        stats_segments(&app.cfg.stats),
        todo_segment(&app.cfg.todos),
        queue_segment(app.prompt_queue.len()),
        hint,
        workspace_status_name(&app.cfg.workspace_name),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            view::truncate_line(&status, left_width),
            theme().dim,
        ))),
        status_area,
    );
}

pub(super) fn transcript_content_width(app: &App, terminal_width: usize) -> usize {
    if app.view_mode == ViewMode::Split && terminal_width >= 100 {
        terminal_width.saturating_mul(58) / 100
    } else {
        terminal_width
    }
}

pub(super) fn stabilize_transcript_scroll(app: &mut App, max_scroll: usize) {
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
/// line stays quiet. `pykernel` is unnumbered (it is 0 or 1).
pub(super) fn stats_segments(stats: &orca_harness_tools::BackgroundStats) -> String {
    let mut out = String::new();
    if stats.processes() > 0 {
        out.push_str(&format!(" · procs {}", stats.processes()));
    }
    if stats.kernels() > 0 {
        out.push_str(" · pykernel");
    }
    if stats.agents() > 0 {
        out.push_str(&format!(" · agents {}", stats.agents()));
    }
    out
}

fn queue_segment(queued: usize) -> String {
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
pub(super) fn mode_segment(mode: &crate::mode::ModeHandle, plan: &crate::plan::PlanArea) -> String {
    if mode.get() == crate::mode::Mode::Normal {
        return String::new();
    }
    match plan.written().len() {
        0 => " · plan mode".to_string(),
        1 => " · plan mode · 1 plan".to_string(),
        n => format!(" · plan mode · {n} plans"),
    }
}

/// Progress through the agent's task list, once it has one.
pub(super) fn todo_segment(todos: &orca_harness_tools::TodoList) -> String {
    match todos.progress() {
        (_, 0) => String::new(),
        (done, total) => format!(" · todo {done}/{total}"),
    }
}

/// A compact execution rail. Prompts stay out of the transcript until
/// they start, so the conversation preserves its actual chronology.
fn queue_lines(app: &App, width: usize) -> Vec<Line<'static>> {
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

/// The pinned live region: approval prompt beats palette beats run status.
/// Streaming content itself is projected into the main transcript.
pub(super) fn live_lines(app: &App, width: usize) -> Vec<Line<'static>> {
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
    if let Some(overlay) = &app.overlay {
        return match overlay {
            Overlay::Models(picker) => model_picker_lines(picker, PICKER_ROWS + 2, width),
            Overlay::Locations(picker) => location_picker_lines(picker, width),
            Overlay::Providers { picker } => provider_lines(picker, width),
            Overlay::Themes { picker } => theme_picker_lines(picker, width),
            Overlay::Views { picker } => view_picker_lines(app.view_mode, picker, width),
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
            lines.push(Line::from(vec![
                Span::styled(format!("  {spinner} "), t.accent),
                Span::styled(
                    format!(
                        "{verb} · {}s · esc to interrupt",
                        started.elapsed().as_secs()
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

/// The task list belongs beside the live run state, where the complete
/// plan stays visible instead of disappearing into a clipped status line.
fn todo_lines(todos: &orca_harness_tools::TodoList, width: usize) -> Vec<Line<'static>> {
    let items = todos.items();
    if items.is_empty() {
        return Vec::new();
    }
    let done = items
        .iter()
        .filter(|item| item.status == TodoStatus::Completed)
        .count();
    let t = theme();
    let mut lines = vec![Line::from(vec![
        Span::styled("  todo", t.strong),
        Span::styled(format!(" · {done}/{} done", items.len()), t.dim),
    ])];
    let last = items.len().saturating_sub(1);
    for (index, item) in items.into_iter().enumerate() {
        let branch = if index == last { "└" } else { "├" };
        let (marker, style) = match item.status {
            TodoStatus::Completed => ("✓", t.dim),
            TodoStatus::InProgress => ("▸", t.strong),
            TodoStatus::Pending => ("□", t.dim),
        };
        let prefix = format!("  {branch} {marker} ");
        let content =
            view::truncate_line(&item.content, width.saturating_sub(prefix.chars().count()));
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(content, style),
        ]));
    }
    lines
}

/// Drop the `@` marker from `@path` mentions before the prompt reaches the
/// model. The marker is composer syntax for the location picker, not part of
/// the path: left in place the model copies it verbatim into tool arguments
/// and every path lookup fails. The composer and transcript keep the `@` so
/// the user still sees what they typed.
pub(crate) fn strip_location_mentions(prompt: &str) -> String {
    let mut out = String::with_capacity(prompt.len());
    let mut rest = prompt;
    while !rest.is_empty() {
        let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (token, tail) = rest.split_at(token_end);
        out.push_str(strip_one_mention(token));

        let gap_end = tail
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(tail.len());
        out.push_str(&tail[..gap_end]);
        rest = &tail[gap_end..];
    }
    out
}

/// A mention is a whole token of the form `@path`. A second `@` means the
/// token is an address or handle (`@user@host`), which is left untouched.
fn strip_one_mention(token: &str) -> &str {
    match token.strip_prefix('@') {
        Some(path) if !path.is_empty() && !path.contains('@') => path,
        _ => token,
    }
}

pub(super) fn mention_starts_at(composer: &str, at: usize) -> bool {
    at == 0
        || composer
            .chars()
            .nth(at.saturating_sub(1))
            .is_some_and(char::is_whitespace)
}

/// Remove the complete `@path` token immediately before the cursor. The
/// picker inserts one trailing space, which is removed with the mention so a
/// single Backspace cleanly undoes the selection.
pub(super) fn remove_location_mention_before_cursor(
    composer: &mut String,
    cursor: &mut usize,
) -> bool {
    if *cursor == 0 {
        return false;
    }
    let chars: Vec<char> = composer.chars().collect();
    let token_end = if chars.get(*cursor - 1).is_some_and(|c| c.is_whitespace()) {
        *cursor - 1
    } else {
        *cursor
    };
    if token_end == 0 {
        return false;
    }
    let token_start = chars[..token_end]
        .iter()
        .rposition(|c| c.is_whitespace())
        .map_or(0, |index| index + 1);
    if chars.get(token_start) != Some(&'@') || token_end == token_start + 1 {
        return false;
    }

    let start_byte = byte_index(composer, token_start);
    let end_byte = byte_index(composer, *cursor);
    composer.replace_range(start_byte..end_byte, "");
    *cursor = token_start;
    true
}

/// Enumerate a bounded set of workspace-relative paths without following
/// symlinks. Build/dependency metadata directories are omitted so `@` stays
/// focused on files an agent can meaningfully work with.
pub(super) fn workspace_locations(root: &Path) -> Vec<LocationEntry> {
    const MAX_LOCATIONS: usize = 5_000;
    const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".next", "dist"];

    fn visit(root: &Path, dir: &Path, entries: &mut Vec<LocationEntry>) {
        if entries.len() >= MAX_LOCATIONS {
            return;
        }
        let Ok(children) = fs::read_dir(dir) else {
            return;
        };
        let mut children: Vec<_> = children.filter_map(Result::ok).collect();
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            if entries.len() >= MAX_LOCATIONS {
                break;
            }
            let Ok(kind) = child.file_type() else {
                continue;
            };
            let name = child.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() && SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            let path: PathBuf = child.path();
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            entries.push(LocationEntry {
                path: relative
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/"),
                directory: kind.is_dir(),
            });
            if kind.is_dir() {
                visit(root, &path, entries);
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort_by(|left, right| {
        right
            .directory
            .cmp(&left.directory)
            .then_with(|| left.path.cmp(&right.path))
    });
    entries
}

fn location_picker_lines(picker: &LocationPicker, width: usize) -> Vec<Line<'static>> {
    let filtered = picker.filtered();
    if filtered.is_empty() {
        return vec![Line::from(Span::styled(
            format!("  No workspace paths match @{} · esc close", picker.query),
            theme().dim,
        ))];
    }
    let header = if picker.query.is_empty() {
        "Files and folders · type to filter · enter add · esc close".to_string()
    } else {
        format!(
            "Files and folders matching @{} · enter add · esc close",
            picker.query
        )
    };
    picker.picker.lines(
        &header,
        filtered.into_iter().map(|entry| {
            if entry.directory {
                format!("{}/", entry.path)
            } else {
                entry.path.clone()
            }
        }),
        width,
    )
}

pub(super) fn projected_transcript(app: &App, width: usize) -> Vec<Line<'static>> {
    projected_transcript_selected(app, width, None)
}

fn projected_transcript_selected(
    app: &App,
    width: usize,
    selected_tool: Option<usize>,
) -> Vec<Line<'static>> {
    let mut lines = app.transcript.clone();
    if !app.running() {
        return lines;
    }

    let activity = activity_lines_selected(app, width, true, selected_tool);
    let answer = if !app.text.is_empty() {
        Some(app.text.as_str())
    } else {
        app.pending_assistant
            .as_deref()
            .filter(|text| !text.is_empty())
    };
    if activity.is_empty() && answer.is_none() {
        return lines;
    }

    let has_activity = !activity.is_empty();
    append_render_block(&mut lines, activity, BlockSpacing::Section, None);
    if let Some(answer) = answer {
        let spacing = if has_activity {
            BlockSpacing::Tight
        } else {
            BlockSpacing::Section
        };
        append_render_block(
            &mut lines,
            view::markdown_lines(answer, width, "  "),
            spacing,
            None,
        );
    }
    lines
}

/// Cap on rendered inner tool rows per spawn while live.
const NESTED_TOOL_ROWS: usize = 4;

/// Inner tool rows for every spawn anchored to `call_id`, plus their
/// descendants. Rows reuse the rail's `├─`/`└─` vocabulary one level
/// deeper, and `continuation` carries the parent rail's `│ ` (or blank)
/// so ownership stays unambiguous even mid-list.
fn nested_subagent_lines(
    app: &App,
    call_id: &str,
    width: usize,
    continuation: &str,
    lines: &mut Vec<Line<'static>>,
) {
    let mut roots: Vec<u64> = app
        .subagent_activity
        .iter()
        .filter(|(_, spawn)| spawn.call_id == call_id)
        .map(|(id, _)| *id)
        .collect();
    roots.sort_unstable();
    let prefix = format!("    {continuation} ");
    for id in roots {
        nested_spawn_rows(app, id, width, &prefix, lines);
    }
}

fn nested_spawn_rows(
    app: &App,
    id: u64,
    width: usize,
    prefix: &str,
    lines: &mut Vec<Line<'static>>,
) {
    let Some(spawn) = app.subagent_activity.get(&id) else {
        return;
    };
    let t = theme();
    let hidden = spawn.tools.len().saturating_sub(NESTED_TOOL_ROWS);
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("{prefix}… {hidden} earlier tools"),
            t.dim,
        )));
    }
    let visible: Vec<&ToolActivity> = spawn.tools.iter().skip(hidden).collect();
    for (position, tool) in visible.iter().enumerate() {
        let last = position + 1 == visible.len();
        let branch = if last { "└─" } else { "├─" };
        let elapsed = tool.elapsed.unwrap_or_else(|| tool.started.elapsed());
        let (glyph, style) = match &tool.output {
            Some(_) if tool.is_error => ("×", t.error),
            Some(_) => ("✓", t.dim),
            None => ("□", t.dim),
        };
        let call = view::truncate_line(
            &tool.call_line,
            width.saturating_sub(prefix.len() + 20).max(8),
        );
        lines.push(Line::from(vec![
            Span::styled(format!("{prefix}{branch} "), t.dim),
            Span::styled(format!("{glyph} "), style),
            Span::styled(call, t.accent),
            Span::styled(format!(" · {}", elapsed_label(elapsed)), t.dim),
        ]));
        // A running nested subagent call: its spawns branch off this row.
        if tool.tool_name == "subagent" && tool.output.is_none() {
            let child_prefix = format!("{prefix}{}  ", if last { " " } else { "│" });
            let mut children: Vec<u64> = app
                .subagent_activity
                .iter()
                .filter(|(_, s)| s.parent_id == Some(id))
                .map(|(child, _)| *child)
                .collect();
            children.sort_unstable();
            for child in children {
                nested_spawn_rows(app, child, width, &child_prefix, lines);
            }
        }
    }
}

/// Quiet, chronological rows for a completed phase. Thinking and tool work
/// remain separate so collapsing detail never rewrites the event sequence.
pub(super) fn collapsed_activity_lines(app: &App) -> Vec<Line<'static>> {
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
        lines.push(Line::from(Span::styled(
            format!(
                "  • Thinking · {} · {}",
                elapsed_label(thinking_elapsed),
                plural(thinking_count, "update")
            ),
            theme().dim,
        )));
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
        lines.push(Line::from(Span::styled(
            format!("  • Work · {}", parts.join(" · ")),
            style,
        )));
    }

    lines
}

/// Render the current run as one coherent activity rail. While the run is
/// live this includes the latest reasoning tail and pending tool states;
/// once committed, the rail is retained for on-demand expansion.
#[cfg(test)]
pub(super) fn activity_lines(app: &App, width: usize, live: bool) -> Vec<Line<'static>> {
    activity_lines_selected(app, width, live, None)
}

pub(super) fn activity_lines_selected(
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
        lines.push(Line::from(Span::styled(
            format!(
                "  {marker} Thinking · {} · {}",
                elapsed_label(elapsed),
                plural(thinking_count, "update")
            ),
            t.dim,
        )));
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
                lines.push(Line::from(Span::styled(format!("    {line}"), t.dim)));
            }
        }
    }

    if app.activity_tools.is_empty() {
        return lines;
    }
    let complete = app
        .activity_tools
        .iter()
        .filter(|tool| tool.output.is_some())
        .count();
    let running = app.activity_tools.len() - complete;
    let header = if live {
        let dot = if app.spinner_frame.is_multiple_of(2) {
            "•"
        } else {
            " "
        };
        format!("  {dot} Work · ✓ {complete} · □ {running}")
    } else {
        format!("  • Work · {}", plural(app.activity_tools.len(), "tool"))
    };
    lines.push(Line::from(Span::styled(header, t.dim)));

    let visible_indices = if live && app.activity_tools.len() > LIVE_TOOL_ROWS {
        let mut selected: Vec<usize> = app
            .activity_tools
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, tool)| tool.output.is_none())
            .map(|(index, _)| index)
            .take(LIVE_TOOL_ROWS)
            .collect();
        let remaining = LIVE_TOOL_ROWS.saturating_sub(selected.len());
        selected.extend(
            app.activity_tools
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, tool)| tool.output.is_some())
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
        lines.push(Line::from(Span::styled(
            format!("    … {hidden} earlier tools"),
            t.dim,
        )));
    }

    for (position, index) in visible_indices.iter().copied().enumerate() {
        let tool = &app.activity_tools[index];
        let last = position + 1 == visible_indices.len();
        let branch = if last { "└─" } else { "├─" };
        let continuation = if last { "  " } else { "│ " };
        let elapsed = tool.elapsed.unwrap_or_else(|| tool.started.elapsed());
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
        let row_width = width.min(132);
        let elapsed = elapsed_label(elapsed);
        let prefix = format!("    {branch} ");
        let detail_width = (row_width / 3).clamp(12, 40);
        detail = view::truncate_line(&detail, detail_width);
        let selected = selected_tool == Some(index);
        let status = if detail.is_empty() {
            format!(" · {elapsed}")
        } else {
            format!(" · {detail} · {elapsed}")
        };
        let fixed_width = prefix.chars().count() + 2 + status.chars().count();
        let connector_reserve = if selected { 10 } else { 0 };
        let call_width = row_width
            .saturating_sub(fixed_width + connector_reserve)
            .max(8);
        let call = view::truncate_line(&tool.call_line, call_width);
        let row_style = if selected { t.select } else { t.accent };
        let mut spans = vec![
            Span::styled(prefix, t.dim),
            Span::styled(format!("{glyph} "), status_style),
            Span::styled(call, row_style),
            Span::styled(status, status_style),
        ];
        if selected {
            let used = spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>();
            let dots = width.saturating_sub(used + 1);
            spans.push(Span::styled(
                format!(" {}○", "·".repeat(dots.saturating_sub(1).max(1))),
                t.dim,
            ));
        }
        lines.push(Line::from(spans));
        if tool.tool_name == "edit_file" {
            lines.extend(edit_diff_preview_lines(tool, width, continuation));
        }
        if tool.tool_name == "subagent" && tool.output.is_none() {
            nested_subagent_lines(app, &tool.call_id, width, continuation, &mut lines);
        }
        if tool.is_error {
            if let Some(output) = &tool.output {
                let output_width = width.saturating_sub(12).max(16);
                for output_line in view::expand_output(&tool.tool_name, output)
                    .into_iter()
                    .take(4)
                {
                    lines.push(Line::from(Span::styled(
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
    lines
}

/// Exact, source-preserving edit context beneath an `edit_file` row. Small
/// edits remain fully visible; large replacements stay bounded so one call
/// cannot take over the live rail.
fn edit_diff_preview_lines(
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
pub(super) fn palette_lines(app: &App, height: usize, width: usize) -> Vec<Line<'static>> {
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

    let selected = app.palette_index.min(filtered.len() - 1);
    let rows = height.saturating_sub(2).max(1);
    let first = selected.saturating_sub(rows.saturating_sub(1));
    let window: Vec<_> = filtered.iter().enumerate().skip(first).take(rows).collect();
    let range = format!(
        "{}-{}",
        first + 1,
        (first + window.len()).min(filtered.len())
    );
    let header_left = format!("  Results {} · type to filter", filtered.len());
    let pad = width
        .saturating_sub(header_left.chars().count() + range.chars().count() + 2)
        .max(1);
    lines.push(Line::from(Span::styled(
        format!("{header_left}{}{range}", " ".repeat(pad)),
        t.dim,
    )));
    lines.push(Line::from(""));

    for (index, spec) in window {
        let is_selected = index == selected;
        let name = format!("/{}", spec.name);
        let left = format!("  {name:<9} {}", spec.description);
        let pad = width
            .saturating_sub(left.chars().count() + spec.category.chars().count() + 2)
            .max(1);
        let (name_style, desc_style) = if is_selected {
            (t.select, Style::default())
        } else {
            (t.dim, t.dim)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {name:<9} "), name_style),
            Span::styled(spec.description.to_string(), desc_style),
            Span::styled(format!("{}{}", " ".repeat(pad), spec.category), t.dim),
        ]));
    }
    lines
}

/// Re-render a recorded transcript into the UI: user turns carry the
/// spine, assistant text lands as markdown, and tool activity collapses
/// to the dim call/result summaries. The live activity rail is not
/// reconstructed — replay is a readable history, not a re-run.
pub(super) fn replay_transcript(
    app: &mut App,
    messages: &[orca_harness_core::Message],
    width: usize,
) {
    for message in messages {
        match message {
            Message::System { .. } => {}
            Message::User { content } => {
                app.push_line(Line::from(""));
                app.push_wrapped(content, "┃ ", theme().strong, width);
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
pub(super) fn reset_conversation_ui(app: &mut App) {
    app.transcript.clear();
    app.pending_history.clear();
    app.scroll = 0;
    app.transcript_max_scroll = 0;
    app.split_inspector_cache = None;
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

fn model_picker_lines(picker: &ModelPicker, height: usize, width: usize) -> Vec<Line<'static>> {
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

    let selected = picker.index.min(filtered.len() - 1);
    let rows = height.saturating_sub(2).max(1);
    let first = selected.saturating_sub(rows.saturating_sub(1));
    let window: Vec<_> = filtered.iter().enumerate().skip(first).take(rows).collect();
    let range = format!(
        "{}-{}",
        first + 1,
        (first + window.len()).min(filtered.len())
    );
    let pad = width
        .saturating_sub(header_left.chars().count() + range.chars().count() + 2)
        .max(1);
    lines.push(Line::from(Span::styled(
        format!("{header_left}{}{range}", " ".repeat(pad)),
        t.dim,
    )));
    lines.push(Line::from(""));

    for (index, model) in window {
        let is_selected = index == selected;
        let marker = if is_selected { "▸ " } else { "  " };
        let style = if is_selected { t.select } else { t.dim };
        let text = format!("  {marker}{}", model.summary());
        lines.push(Line::from(Span::styled(
            view::truncate_line(&text, width),
            style,
        )));
    }
    lines
}

/// The status-line context segment: percentage of the model's window when
/// known (`ctx 33%`), a plain count only when no window is discoverable.
/// Exact figures live behind /usage, never here.
pub(super) fn context_segment(tokens: u64, window: Option<u64>) -> String {
    match window {
        Some(window) if window > 0 => {
            format!("ctx {}%", (100 * tokens / window).min(999))
        }
        _ => format!("ctx ~{}", fmt_tokens(tokens)),
    }
}

fn provider_lines(picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let rows = Provider::ALL.iter().map(|provider| {
        let key_note = match provider.key_env() {
            None => "no key needed".to_string(),
            Some(env) if provider.env_key().is_some() => format!("key from ${env}"),
            Some(env) => format!("${env} not set — will ask"),
        };
        format!(
            "{:<12} {:<36} {key_note}",
            provider.label(),
            provider.base_url()
        )
    });
    picker.lines(
        "Select provider · ↑↓ navigate · enter use · esc close",
        rows,
        width,
    )
}

/// The /usage panel: same tray styling as the provider and theme
/// pickers, but read-only — nothing to select, esc/enter/q closes.
fn usage_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let total = app.tokens_in + app.tokens_out + app.cache_read_total + app.cache_write_total;
    let context = match app.context_window {
        Some(window) if window > 0 => format!(
            "{} / {} ({}%)",
            app.context_tokens,
            window,
            (100 * app.context_tokens / window).min(999),
        ),
        _ => format!("~{} (window unknown)", app.context_tokens),
    };
    let mut lines = vec![
        Line::from(Span::styled("  Session usage · esc close", t.dim)),
        Line::from(""),
    ];
    let rows = [
        ("model", app.cfg.model_name.clone()),
        ("context", context),
        ("input", app.tokens_in.to_string()),
        ("output", app.tokens_out.to_string()),
        ("cache read", app.cache_read_total.to_string()),
        ("cache write", app.cache_write_total.to_string()),
        ("total", total.to_string()),
        ("model steps", app.usage_steps.to_string()),
    ];
    for (label, value) in rows {
        let text = format!("  {label:<12} {value}");
        lines.push(Line::from(Span::styled(
            view::truncate_line(&text, width),
            t.dim,
        )));
    }
    lines
}

fn theme_picker_lines(picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let current = view::theme_name();
    let rows = view::ThemeName::ALL.iter().map(|name| {
        let note = if *name == current { "current" } else { "" };
        format!("{:<16} {:<16} {note}", name.label(), name.slug())
    });
    picker.lines(
        "Select theme · ↑↓ navigate · enter use · esc close",
        rows,
        width,
    )
}

fn view_picker_lines(current: ViewMode, picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let rows = ViewMode::ALL.iter().map(|mode| {
        let note = if *mode == current { "current" } else { "" };
        let description = match mode {
            ViewMode::Classic => "transcript with inline work rail",
            ViewMode::Split => "tool rail with connected inspector",
        };
        format!("{:<10} {:<38} {note}", mode.label(), description)
    });
    picker.lines(
        "Select view · ↑↓ navigate · enter use · esc close",
        rows,
        width,
    )
}

/// The /settings tray: current values for the persisted preferences,
/// enter drills into the matching picker.
fn settings_lines(app: &App, picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let provider = app.cfg.provider;
    let key_status = match provider.key_env() {
        None => "not needed".to_string(),
        Some(env) if provider.env_key().is_some() => format!("from ${env}"),
        Some(_) if provider.stored_key().is_some() => "saved in config".to_string(),
        Some(_) => "not set".to_string(),
    };
    let approvals = crate::config::stored_approvals(&app.cfg.workspace_root);
    let approvals_status = if approvals.is_empty() {
        "none saved".to_string()
    } else {
        approvals.join(", ")
    };
    let rows = [
        ("provider", provider.label().to_string()),
        ("model", app.cfg.model_name.clone()),
        ("theme", view::theme_name().label().to_string()),
        ("view", app.view_mode.label().to_string()),
        ("api key", key_status),
        ("approvals", approvals_status),
    ];
    let mut lines = picker.lines(
        "Settings · ↑↓ navigate · enter change · esc close",
        rows.iter()
            .map(|(name, value)| format!("{name:<10} {value}")),
        width,
    );
    if let Some(path) = crate::config::config_path() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            view::truncate_line(&format!("  saved to {}", path.display()), width),
            t.dim,
        )));
    }
    lines
}

/// This workspace's saved always-allowed tools; enter revokes the
/// selected one so it prompts again.
fn approvals_lines(tools: &[String], picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    picker.lines(
        "Saved approvals (this workspace) · ↑↓ navigate · enter revoke · esc close",
        tools.iter().cloned(),
        width,
    )
}

/// The harness extension catalog with live on/off state; enter toggles
/// the selected extension and the list stays open.
fn extensions_picker_lines(picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let rows = crate::extensions::EXTENSIONS.iter().map(|spec| {
        let state = if crate::extensions::is_enabled(spec) {
            "on "
        } else {
            "off"
        };
        format!("{:<12} {state}  {}", spec.name, spec.description)
    });
    picker.lines(
        "Extensions · ↑↓ navigate · enter toggle · esc close",
        rows,
        width,
    )
}

/// The configured MCP servers: state, tool count, launch command.
///
/// On/off comes from `servers` (the overlay's own copy, updated the
/// instant the toggle is saved) so a press redraws now; the tool count
/// comes from the shared handle and lags by one reconnect, showing `…`
/// until the worker reports. A server that failed to connect shows why
/// instead of a count. Commands are redacted: the config may hold a
/// literal token, and this list is the one place it would be on screen.
fn mcp_picker_lines(
    servers: &[crate::config::McpServer],
    mcp: &crate::mcp::McpServers,
    filter: &str,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let name_width = servers
        .iter()
        .map(|server| server.name.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(4, 16);
    let indices = matching_indices(servers, filter, |server| &server.name);
    let rows = indices.into_iter().map(|index| {
        let server = &servers[index];
        let (count, why) = if !server.enabled {
            (String::new(), String::new())
        } else {
            match mcp.state(&server.name) {
                Some(crate::mcp::McpState::Connected(1)) => ("1 tool".into(), String::new()),
                Some(crate::mcp::McpState::Connected(n)) => (format!("{n} tools"), String::new()),
                // The reason goes after the command, not in the count
                // column: an error is far too long to keep the columns
                // aligned, and it would push the command off the row.
                Some(crate::mcp::McpState::Failed(err)) => ("failed".into(), format!("  — {err}")),
                None => ("…".into(), String::new()),
            }
        };
        let state = if server.enabled { "on " } else { "off" };
        format!(
            "{:<name_width$}  {state}  {:<9}  {}{why}",
            server.name,
            count,
            redact_command(&server.command)
        )
    });
    let filter_note = if filter.is_empty() {
        "type to filter".into()
    } else {
        format!("filter: {filter}")
    };
    picker.windowed_lines(
        &format!("MCP servers · {filter_note} · ↑↓ navigate · space toggle · esc close"),
        rows,
        width,
        PICKER_ROWS,
    )
}

/// The skills found on disk: state, where each came from, and what it
/// is for.
///
/// On/off comes from `entries` (the overlay's own copy, updated the
/// instant the toggle is saved) so a press redraws now. Rows that could
/// not load, or that lost a name collision to an earlier root, are
/// listed too — a skill that silently is not there is the failure mode
/// worth spending a row on.
fn skills_picker_lines(
    entries: &[crate::skills::SkillEntry],
    filter: &str,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let name_width = entries
        .iter()
        .map(|entry| entry.name.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(4, 20);
    let indices = matching_indices(entries, filter, |entry| &entry.name);
    let rows = indices.into_iter().map(|index| {
        let entry = &entries[index];
        let (state, detail) = match &entry.state {
            crate::skills::SkillState::Loaded { root, bytes } => (
                if entry.enabled { "on " } else { "off" },
                format!("{root}  {}  {}", size(*bytes), entry.description),
            ),
            crate::skills::SkillState::Shadowed { root, by } => {
                ("—  ", format!("{root}  shadowed by {by}"))
            }
            crate::skills::SkillState::Failed { root, reason } => {
                ("—  ", format!("{root}  failed — {reason}"))
            }
        };
        format!("{:<name_width$}  {state}  {detail}", entry.name)
    });
    let filter_note = if filter.is_empty() {
        "type to filter".into()
    } else {
        format!("filter: {filter}")
    };
    picker.windowed_lines(
        &format!("Skills · {filter_note} · ↑↓ navigate · enter toggle · esc close"),
        rows,
        width,
        PICKER_ROWS,
    )
}

pub(super) fn matching_indices<T, F>(items: &[T], filter: &str, label: F) -> Vec<usize>
where
    F: Fn(&T) -> &str,
{
    let needle = filter.to_lowercase();
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| needle.is_empty() || label(item).to_lowercase().contains(&needle))
        .map(|(index, _)| index)
        .collect()
}

/// Recorded sessions for this workspace, newest first; enter resumes
/// the selected one. Same navigation grammar as the /models picker:
/// the newest sessions fill a bounded window, and ↑↓/PgUp/PgDn move
/// the cursor through the entire list with the position in the header.
fn sessions_picker_lines(
    sessions: &[orca_harness_extensions::SessionFile],
    current: Option<&str>,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let rows = sessions.iter().map(|session| {
        let note = if current == Some(session.meta.id.as_str()) {
            "  (current)"
        } else {
            ""
        };
        format!(
            "{}  {:<8} {}{note}",
            session.meta.id,
            age_label(session.meta.created_at),
            session.meta.model,
        )
    });
    picker.windowed_lines(
        "Sessions (this workspace) · ↑↓ navigate · PgUp/PgDn page · enter resume · esc close",
        rows,
        width,
        SESSIONS_WINDOW,
    )
}

fn api_key_lines(provider: Provider, input: &str) -> Vec<Line<'static>> {
    let t = theme();
    vec![
        Line::from(Span::styled(
            format!(
                "  {} API key (saved for future sessions) · enter confirm · esc cancel",
                provider.label()
            ),
            t.warn,
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  key: ", t.dim),
            Span::raw("•".repeat(input.chars().count())),
        ]),
    ]
}
