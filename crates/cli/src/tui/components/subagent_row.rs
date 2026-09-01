//! Compact provider/model-first row for delegated agents.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::tree::{finish_row, row_budget, Connector, TreeBranch};
use crate::view;

pub struct SubagentRow<'a> {
    pub branch: TreeBranch<'a>,
    pub glyph: &'a str,
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
        let fixed = view::cell_width(&prefix)
            + view::cell_width(self.glyph)
            + view::cell_width(" subagent ·  · ")
            + view::cell_width(self.elapsed);
        let content_width = row_width.saturating_sub(fixed).max(8);
        let identity_width = view::cell_width(self.identity).min(content_width);
        let task_width = content_width.saturating_sub(identity_width + 3);
        let identity = view::truncate_line(self.identity, identity_width.max(8));
        let task = (task_width >= 8).then(|| view::truncate_line(self.task, task_width));

        let mut spans = vec![
            Span::styled(prefix, self.branch_style),
            Span::styled(format!("{} ", self.glyph), self.glyph_style),
            Span::styled("subagent", self.label_style),
            Span::styled(" · ", self.branch_style),
            Span::styled(identity, self.identity_style),
        ];
        if let Some(task) = task.filter(|task| !task.is_empty()) {
            spans.push(Span::styled(" · ", self.branch_style));
            spans.push(Span::styled(task, self.task_style));
        }
        spans.push(Span::styled(
            format!(" · {}", self.elapsed),
            self.glyph_style,
        ));
        finish_row(spans, self.width, self.connector, self.branch_style)
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
    fn renders_identity_before_task() {
        assert_eq!(
            text(&row(120).line()),
            "    └─ ✓ subagent · openrouter:anthropic/claude-sonnet-5 · Explore the benchmarks directory · 37.0s"
        );
    }

    #[test]
    fn drops_task_before_identity_when_narrow() {
        let rendered = text(&row(70).line());
        assert!(
            rendered.contains("subagent · openrouter:anthropic/claude-sonnet-5"),
            "{rendered}"
        );
        assert!(!rendered.contains("Explore"), "{rendered}");
    }
}
