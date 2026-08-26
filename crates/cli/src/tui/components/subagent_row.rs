//! Compact provider/model-first row for delegated agents.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::view;

pub struct SubagentRow<'a> {
    pub last: bool,
    pub prefix: &'a str,
    pub glyph: &'a str,
    pub identity: &'a str,
    pub task: &'a str,
    pub elapsed: &'a str,
    pub selected: bool,
    pub width: usize,
    pub branch_style: Style,
    pub glyph_style: Style,
    pub label_style: Style,
    pub identity_style: Style,
    pub task_style: Style,
}

impl SubagentRow<'_> {
    pub fn continuation(&self) -> &'static str {
        if self.last {
            "  "
        } else {
            "│ "
        }
    }

    pub fn line(&self) -> Line<'static> {
        let branch = if self.last { "└─" } else { "├─" };
        let prefix = format!("{}{branch} ", self.prefix);
        let row_width = self.width.min(132);
        let connector_reserve = if self.selected { 10 } else { 0 };
        let fixed = prefix.chars().count()
            + self.glyph.chars().count()
            + " subagent ·  · ".chars().count()
            + self.elapsed.chars().count()
            + connector_reserve;
        let content_width = row_width.saturating_sub(fixed).max(8);
        let identity_width = self.identity.chars().count().min(content_width);
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
        if self.selected {
            let used = spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>();
            let dots = self.width.saturating_sub(used + 1);
            spans.push(Span::styled(
                format!(" {}○", "·".repeat(dots.saturating_sub(1).max(1))),
                self.branch_style,
            ));
        }
        Line::from(spans)
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
            last: true,
            prefix: "    ",
            glyph: "✓",
            identity: "openrouter:anthropic/claude-sonnet-5",
            task: "Explore the benchmarks directory",
            elapsed: "37.0s",
            selected: false,
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
