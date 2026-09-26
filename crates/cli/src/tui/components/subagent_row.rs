//! Compact task-first row for delegated agents.
//!
//! What the agent is doing is the first question a glance asks; which
//! model does it is the second. The task comes first, then the model
//! shortened to its name, then the time; the inspector keeps the full
//! `provider:vendor/model`.
//!
//! `label` names the kind of work the row stands for — a spawned `Subagent`,
//! or a `Stage` of a submitted graph — so both read as one family and one
//! width calculation serves them.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::tree::{finish_row, row_budget, Connector, TreeBranch};
use crate::view;

pub struct SubagentRow<'a> {
    pub branch: TreeBranch<'a>,
    pub glyph: &'a str,
    /// The row's kind: `"Subagent"`, `"Stage"`.
    pub label: &'a str,
    pub identity: &'a str,
    pub task: &'a str,
    pub elapsed: &'a str,
    pub connector: Connector,
    pub width: usize,
    pub branch_style: Style,
    pub glyph_style: Style,
    pub label_style: Style,
    pub identity_style: Style,
    pub task_style: Style,
}

impl SubagentRow<'_> {
    pub fn continuation(&self) -> &'static str {
        self.branch.continuation()
    }

    pub fn line(&self) -> Line<'static> {
        let prefix = self.branch.prefix();
        let row_width = row_budget(self.width, self.connector);
        let identity = short_identity(self.identity);
        let fixed = view::cell_width(&prefix)
            + view::cell_width(self.glyph)
            + 1
            + view::cell_width(self.label)
            + view::cell_width(SEP)
            + if self.elapsed.is_empty() {
                0
            } else {
                view::cell_width(SEP) + view::cell_width(self.elapsed)
            };
        let available = row_width.saturating_sub(fixed);
        // The model gives way before the task does: it only shows while
        // the task keeps a readable share of the row.
        let task_floor = view::cell_width(self.task).min(24);
        let identity_cost = view::cell_width(SEP) + view::cell_width(&identity);
        let identity =
            (!identity.is_empty() && available >= task_floor + identity_cost).then_some(identity);
        let task_width = available.saturating_sub(identity.as_ref().map_or(0, |_| identity_cost));

        let mut spans = vec![
            Span::styled(prefix, self.branch_style),
            Span::styled(format!("{} ", self.glyph), self.glyph_style),
            Span::styled(self.label.to_string(), self.label_style),
            Span::styled(SEP, self.branch_style),
            Span::styled(view::truncate_line(self.task, task_width), self.task_style),
        ];
        if let Some(identity) = identity {
            spans.push(Span::styled(SEP, self.branch_style));
            spans.push(Span::styled(identity, self.identity_style));
        }
        if !self.elapsed.is_empty() {
            spans.push(Span::styled(
                format!("{SEP}{}", self.elapsed),
                self.branch_style,
            ));
        }
        finish_row(spans, self.width, self.connector, self.branch_style)
    }
}

const SEP: &str = " · ";

/// `openrouter:anthropic/claude-sonnet-5` → `claude-sonnet-5`. Anything
/// that is not a `provider:model` route passes through unchanged.
pub fn short_identity(identity: &str) -> String {
    let identity = view::sanitize_cells(identity);
    if identity.contains(char::is_whitespace) {
        return identity.into_owned();
    }
    match identity.split_once(':') {
        Some((_, model)) => model.rsplit('/').next().unwrap_or(model).to_string(),
        None => identity.into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn row(width: usize) -> SubagentRow<'static> {
        SubagentRow {
            branch: TreeBranch {
                indent: "    ",
                last: true,
            },
            glyph: "✓",
            label: "Subagent",
            identity: "openrouter:anthropic/claude-sonnet-5",
            task: "Explore the benchmarks directory",
            elapsed: "37.0s",
            connector: Connector::None,
            width,
            branch_style: Style::default(),
            glyph_style: Style::default(),
            label_style: Style::default(),
            identity_style: Style::default(),
            task_style: Style::default(),
        }
    }

    #[test]
    fn renders_task_before_a_short_model_and_the_time() {
        let rendered = text(&row(120).line());
        assert!(
            rendered.starts_with("    └─ ✓ Subagent · Explore the benchmarks directory"),
            "{rendered:?}"
        );
        assert!(
            rendered.ends_with(" · claude-sonnet-5 · 37.0s"),
            "{rendered:?}"
        );
        assert!(!rendered.contains("openrouter"), "{rendered:?}");
    }

    #[test]
    fn drops_the_model_before_the_task_when_narrow() {
        let rendered = text(&row(58).line());
        assert!(rendered.contains("Explore the benchmarks"), "{rendered}");
        assert!(!rendered.contains("sonnet"), "{rendered}");
        assert!(rendered.ends_with("37.0s"), "{rendered}");
    }
}
