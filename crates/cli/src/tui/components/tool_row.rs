//! One atomic row in a Work rail.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::tree::{finish_row, row_budget, Connector, TreeBranch};
use crate::view;

pub struct ToolRow<'a> {
    pub branch: TreeBranch<'a>,
    pub glyph: &'a str,
    pub call: &'a str,
    pub detail: &'a str,
    pub elapsed: &'a str,
    pub connector: Connector,
    pub width: usize,
    pub branch_style: Style,
    pub glyph_style: Style,
    pub call_style: Style,
}

impl ToolRow<'_> {
    pub fn continuation(&self) -> &'static str {
        self.branch.continuation()
    }

    pub fn line(&self) -> Line<'static> {
        let prefix = self.branch.prefix();
        let row_width = row_budget(self.width, self.connector);
        let detail_width = (row_width / 3).clamp(12, 40);
        let detail = view::truncate_line(self.detail, detail_width);
        let status: String = [detail.as_str(), self.elapsed]
            .into_iter()
            .filter(|part| !part.is_empty())
            .map(|part| format!(" · {part}"))
            .collect();
        let fixed_width = view::cell_width(&prefix) + 2 + view::cell_width(&status);
        let call_width = row_width.saturating_sub(fixed_width).max(8);
        let spans = vec![
            Span::styled(prefix, self.branch_style),
            Span::styled(format!("{} ", self.glyph), self.glyph_style),
            Span::styled(view::truncate_line(self.call, call_width), self.call_style),
            Span::styled(status, self.glyph_style),
        ];
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

    fn row(width: usize, connector: Connector) -> ToolRow<'static> {
        ToolRow {
            branch: TreeBranch {
                indent: "    ",
                last: true,
            },
            glyph: "✓",
            call: "read_file README.md",
            detail: "read 120 bytes",
            elapsed: "2ms",
            connector,
            width,
            branch_style: Style::default(),
            glyph_style: Style::default(),
            call_style: Style::default(),
        }
    }

    #[test]
    fn renders_branch_call_detail_and_elapsed_as_one_compact_row() {
        let row = row(100, Connector::None);
        assert_eq!(
            text(&row.line()),
            "    └─ ✓ read_file README.md · read 120 bytes · 2ms"
        );
        assert_eq!(row.continuation(), "  ");
    }

    #[test]
    fn a_reserved_row_truncates_like_the_drawn_one_so_the_column_stays_straight() {
        let mut long = row(40, Connector::Reserved);
        long.call = "read_file crates/cli/src/tui/components/tool_row.rs";
        let reserved = text(&long.line());
        long.connector = Connector::Drawn;
        let drawn = text(&long.line());
        assert!(drawn.starts_with(&reserved), "{reserved:?} vs {drawn:?}");
        assert!(drawn.ends_with('○'));
    }

    #[test]
    fn wide_glyphs_count_as_two_cells() {
        let mut row = row(30, Connector::None);
        row.call = "read_file 日本語のファイル名.md";
        row.detail = "";
        assert!(row.line().width() <= 30);
    }
}
