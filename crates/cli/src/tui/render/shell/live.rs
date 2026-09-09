//! Status segments and the pinned queue, overlay, and run-state region.

use ratatui::text::{Line, Span};

use crate::tui::components::approval::ApprovalPrompt;
use crate::tui::components::tree::TreeBranch;
use crate::tui::format::{elapsed_label, fmt_tokens};
use crate::tui::{App, Overlay, RunState, PALETTE_ROWS, PICKER_ROWS, QUEUE_PREVIEW_ROWS};
use crate::view::glyphs::glyphs;
use crate::view::{self, theme};

use super::super::overlays::*;
use super::super::pickers::*;
use super::super::plugins::*;
use super::super::transcript::{
    location_picker_lines, process_lines, skill_mention_picker_lines, todo_lines,
};

/// Status-line segments for live background work; empty when idle so the
/// line stays quiet. Each persistent compute tool is unnumbered (0 or 1).
pub(crate) fn stats_segments(
    stats: &orca_harness_tools::BackgroundStats,
    active_agents: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    if stats.processes() > 0 {
        let g = glyphs();
        out.push(g.counted(g.process, "procs", &stats.processes().to_string()));
    }
    if stats.kernels() > 0 {
        out.push("pykernel".to_string());
    }
    if stats.bun_repls() > 0 {
        out.push("bun".to_string());
    }
    let agents = active_agents.max(stats.agents());
    if agents > 0 {
        out.push(format!("agents {agents} ↓"));
    }
    out
}

pub(crate) fn queue_segment(queued: usize) -> String {
    if queued == 0 {
        String::new()
    } else {
        let g = glyphs();
        g.counted(g.waiting, "q", &queued.to_string())
    }
}

/// Where the session is running: the workspace root and, when the
/// checkout has one, its branch. Returned as (full, compact) so the bar
/// can shed the folder name before it sheds anything that carries a
/// number.
pub(crate) fn workspace_segment(workspace: &str, branch: Option<&str>) -> (String, String) {
    let g = glyphs();
    let name = crate::tui::format::workspace_status_name(workspace);
    (
        g.workspace_label(name, branch),
        g.workspace_compact(name, branch),
    )
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
        0 => "plan".to_string(),
        1 => "plan · 1 plan".to_string(),
        n => format!("plan · {n} plans"),
    }
}

/// Progress through the agent's task list, once it has one.
pub(crate) fn todo_segment(todos: &orca_harness_tools::TodoList) -> String {
    match todos.progress() {
        (_, 0) => String::new(),
        (done, total) => {
            let g = glyphs();
            g.counted(g.done, "todo", &format!("{done}/{total}"))
        }
    }
}

/// A compact execution rail. Prompts stay out of the transcript until
/// they start, so the conversation preserves its actual chronology.
pub(crate) fn queue_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let g = glyphs();
    if app.prompt_queue.is_empty() {
        return Vec::new();
    }

    let mut lines = vec![Line::from(vec![
        Span::styled(format!("  {}", g.status_prefix(g.waiting)), t.dim),
        Span::styled("queued", t.strong),
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
#[cfg(test)]
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
            Overlay::Style { picker } => style_lines(picker, width),
            Overlay::Inspector { picker } => {
                inspector_picker_lines(app.inspector_mode, picker, width)
            }
            Overlay::Usage => usage_lines(app, width),
            Overlay::Todo => todo_lines(&app.cfg.todos, width),
            Overlay::Processes => process_lines(&app.cfg.stats, width),
            Overlay::ApiKey { provider, input } => api_key_lines(*provider, input),
            Overlay::Settings { picker } => settings_lines(app, picker, width),
            Overlay::Subagents { picker } => subagent_settings_lines(app, picker, width),
            Overlay::SubagentValues {
                setting,
                values,
                picker,
            } => subagent_value_lines(*setting, values, picker, width),
            Overlay::SubagentNumber {
                setting,
                input,
                error,
            } => {
                let field = setting.numeric().expect("numeric editor");
                vec![
                    Line::from(format!(
                        "Subagent {} · enter save · ← back · esc close",
                        field.label
                    )),
                    Line::from(format!(
                        "Current: {} · {}",
                        field.current(&app.cfg.subagent_depth),
                        field.unit
                    )),
                    Line::from(if field.unlimited {
                        "Enter a number; 0 or unlimited removes the limit"
                    } else {
                        "Enter a number"
                    }),
                    Line::from(format!("> {input}")),
                    Line::from(error.clone()),
                    Line::from(setting.applies()),
                ]
            }
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
        let spinner = glyphs().active_frame(app.spinner_frame);
        let verb = if !app.text.is_empty() || app.pending_assistant.is_some() {
            "writing"
        } else if !app.reasoning.is_empty() {
            "thinking"
        } else {
            "working"
        };
        if let RunState::Running { started, .. } = &app.run {
            let mut token_io = if app.turn_tokens_in == 0 && app.turn_tokens_out == 0 {
                String::new()
            } else {
                format!(
                    " · ↑{} ↓{}",
                    fmt_tokens(app.turn_tokens_in),
                    fmt_tokens(app.turn_tokens_out)
                )
            };
            if let Some(tokens) = app.turn_thinking_tokens {
                token_io.push_str(&format!(" · thinking {}", fmt_tokens(tokens)));
            }
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
    let lines = queue_lines(app, width);
    LiveRegion {
        lines,
        anchor: LiveAnchor::Bottom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
