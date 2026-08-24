//! One atomic row in a Work rail.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::view;

pub struct ToolRow<'a> {
    pub last: bool,
    pub glyph: &'a str,
    pub call: &'a str,
    pub detail: &'a str,
    pub elapsed: &'a str,
    pub selected: bool,
    pub width: usize,
    pub branch_style: Style,
    pub glyph_style: Style,
    pub call_style: Style,
}

impl ToolRow<'_> {
    pub fn continuation(&self) -> &'static str {
        if self.last {
            "  "
        } else {
            "│ "
        }
    }

    pub fn line(&self) -> Line<'static> {
        let branch = if self.last { "└─" } else { "├─" };
        let prefix = format!("    {branch} ");
        let row_width = self.width.min(132);
        let detail_width = (row_width / 3).clamp(12, 40);
        let detail = view::truncate_line(self.detail, detail_width);
        let status = if detail.is_empty() {
            format!(" · {}", self.elapsed)
        } else {
            format!(" · {detail} · {}", self.elapsed)
        };
        let fixed_width = prefix.chars().count() + 2 + status.chars().count();
        let connector_reserve = if self.selected { 10 } else { 0 };
        let call_width = row_width
            .saturating_sub(fixed_width + connector_reserve)
            .max(8);
        let mut spans = vec![
            Span::styled(prefix, self.branch_style),
            Span::styled(format!("{} ", self.glyph), self.glyph_style),
            Span::styled(view::truncate_line(self.call, call_width), self.call_style),
            Span::styled(status, self.glyph_style),
        ];
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

    #[test]
    fn renders_branch_call_detail_and_elapsed_as_one_compact_row() {
        let row = ToolRow {
            last: true,
            glyph: "✓",
            call: "read_file README.md",
            detail: "read 120 bytes",
            elapsed: "2ms",
            selected: false,
            width: 100,
            branch_style: Style::default(),
            glyph_style: Style::default(),
            call_style: Style::default(),
        };

        assert_eq!(
            text(&row.line()),
            "    └─ ✓ read_file README.md · read 120 bytes · 2ms"
        );
        assert_eq!(row.continuation(), "  ");
    }
}
