//! The submitted graph of a workflow run, drawn in the agent browser's
//! transcript pane.
//!
//! A run has no transcript of its own — it is an overlay that drives ordinary
//! subagents — so the pane shows what the run actually is: every stage of the
//! graph, from submission, with the ones that have spawned joined to their
//! agents. Marks and colours come from the same tables as agent rows, and the
//! rows are [`SubagentRow`]s, so a stage and the agent running it read alike.

use ratatui::style::Style;
use ratatui::text::Line;

use crate::tui::components::subagent_row::SubagentRow;
use crate::tui::components::tree::{Connector, TreeBranch};
use crate::tui::format::elapsed_label;
use crate::tui::state::workflow::{StageRow, StageState};
use crate::tui::state::App;
use crate::view::glyphs::glyphs;
use crate::view::{self, theme};

use super::agent_line;

/// A stage's mark, from the same table as tool and agent rows: a waiting
/// stage reads as a waiting agent, not as its own vocabulary.
fn stage_mark(state: StageState) -> char {
    let g = glyphs();
    match state {
        // Nothing is executing yet in either case; the row's detail says
        // which, exactly as a queued agent row does.
        StageState::Blocked | StageState::Queued | StageState::Stopped => '-',
        StageState::Running => g.waiting,
        StageState::Done => g.done,
        StageState::Failed => g.failed,
    }
}

fn stage_style(state: StageState) -> Style {
    let t = theme();
    match state {
        StageState::Blocked | StageState::Queued | StageState::Stopped => t.dim,
        StageState::Running => t.accent,
        StageState::Done => t.success,
        StageState::Failed => t.error,
    }
}

/// Whether the selected agent is a workflow run whose graph we hold.
pub(super) fn is_workflow(app: &App, id: Option<u64>) -> bool {
    id.is_some_and(|id| app.workflows.contains_key(&id))
}

/// The pinned header: what the run is, and where it has got to.
pub(super) fn header_lines(app: &App, id: u64, width: usize, frame: usize) -> Vec<Line<'static>> {
    let Some(run) = app.workflows.get(&id) else {
        return Vec::new();
    };
    let t = theme();
    let counts = run.counts();
    let g = glyphs();
    // Only a section header carries motion; rows never do.
    let mark = if run.is_active() {
        g.active_frame(frame)
    } else {
        g.section
    };
    let elapsed = elapsed_label(run.elapsed.unwrap_or_else(|| run.started.elapsed()));
    let mut lines = vec![agent_line(
        &format!(
            "{mark} Workflow #{id} · {} stages · {elapsed}",
            counts.total()
        ),
        width,
        t.strong,
    )];
    let mut summary = format!("{}/{} done", counts.done, counts.total());
    if counts.running > 0 {
        summary.push_str(&format!(" · {} running", counts.running));
    }
    if counts.waiting > 0 {
        summary.push_str(&format!(" · {} waiting", counts.waiting));
    }
    if counts.failed > 0 {
        summary.push_str(&format!(" · {} failed", counts.failed));
    }
    if counts.stopped > 0 {
        summary.push_str(&format!(" · {} stopped", counts.stopped));
    }
    lines.push(agent_line(
        &summary,
        width,
        if counts.failed > 0 { t.error } else { t.dim },
    ));
    if let Some(error) = &run.error {
        lines.extend(super::agent_text(error, width, t.error));
    }
    lines
}

/// One row per stage, in submitted order. Rows are the graph, not the spawn
/// stream: a stage that has never run still has a row.
pub(super) fn body_lines(app: &App, id: u64, width: usize) -> Vec<Line<'static>> {
    let Some(run) = app.workflows.get(&id) else {
        return Vec::new();
    };
    let t = theme();
    let mut lines = Vec::with_capacity(run.stages.len() + 2);
    // The tray rhythm: header, one blank row, rows.
    lines.push(Line::from(""));
    lines.push(agent_line("stages", width, t.strong));
    let last = run.stages.len().saturating_sub(1);
    for (index, stage) in run.stages.iter().enumerate() {
        lines.push(stage_line(stage, index == last, width));
    }
    lines
}

fn stage_line(stage: &StageRow, last: bool, width: usize) -> Line<'static> {
    let t = theme();
    let style = stage_style(stage.state);
    let mark = stage_mark(stage.state).to_string();
    // The detail column says why a stage waits, or what went wrong; only a
    // stage with nothing to report falls back to the prompt it was given.
    let detail = stage.detail.clone().unwrap_or_else(|| {
        let prompt = view::sanitize_cells(&stage.prompt);
        if prompt.trim().is_empty() {
            stage.state.label().to_string()
        } else {
            prompt.into_owned()
        }
    });
    // The identity column is the stage's model when it named one. A stage
    // that inherits says nothing there rather than repeating its own state.
    let mut identity = stage.model.clone().unwrap_or_default();
    if stage.is_map {
        identity = if identity.is_empty() {
            "map".into()
        } else {
            format!("{identity} · map")
        };
    }
    let task = if identity.is_empty() {
        detail
    } else {
        format!("{identity} · {detail}")
    };
    SubagentRow {
        branch: TreeBranch { indent: "  ", last },
        glyph: &mark,
        label: "Stage",
        identity: &stage.id,
        task: &task,
        elapsed: &stage.elapsed().map(elapsed_label).unwrap_or_default(),
        connector: Connector::None,
        width,
        branch_style: t.dim,
        glyph_style: style,
        label_style: t.dim,
        identity_style: style,
        task_style: t.dim,
    }
    .line()
}

/// The status bar's run summary, so the browser's footer describes the
/// selected run rather than the session's agents.
pub(super) fn status_summary(app: &App, id: u64) -> Option<String> {
    let run = app.workflows.get(&id)?;
    let counts = run.counts();
    Some(format!(
        "workflow #{id} {}/{} stages",
        counts.done,
        counts.total()
    ))
}
