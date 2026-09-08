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
        let call = view::sanitize_cells(self.call);
        let call = call.lines().next().unwrap_or_default();
        let (name, target) = call.split_once(' ').unwrap_or((call, ""));
        let mapped_action = crate::presentation::tool_action_label(name);
        let action = if mapped_action.is_empty() {
            name
        } else {
            mapped_action
        };
        let action = if target.is_empty() {
            action.to_string()
        } else {
            format!("{action} · ")
        };
        let prefix_width = view::cell_width(&prefix) + view::cell_width(self.glyph) + 1;
        let elapsed_width = if self.elapsed.is_empty() {
            0
        } else {
            view::cell_width(self.elapsed) + 3
        };
        let available =
            row_width.saturating_sub(prefix_width + view::cell_width(&action) + elapsed_width);
        // The target gets priority over the result summary on narrow panes.
        let detail_width = (row_width / 3)
            .min(40)
            .min(available.saturating_sub(view::cell_width(target).min(28) + 3));
        let detail = if detail_width == 0 {
            String::new()
        } else {
            view::truncate_line(self.detail, detail_width)
        };
        let status = if detail.is_empty() {
            String::new()
        } else {
            format!(" · {detail}")
        };
        let target_width = available.saturating_sub(view::cell_width(&status));
        let mut spans = vec![
            Span::styled(prefix, self.branch_style),
            Span::styled(format!("{} ", self.glyph), self.glyph_style),
            Span::styled(action, self.branch_style),
            Span::styled(view::truncate_line(target, target_width), self.call_style),
            Span::styled(status, self.branch_style),
        ];
        if !self.elapsed.is_empty() {
            spans.push(Span::styled(
                format!(" · {}", self.elapsed),
                self.branch_style,
            ));
        }
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
        let rendered = text(&row.line());
        assert!(rendered.starts_with("    └─ ✓ Read · README.md · read 120 bytes"));
        assert!(rendered.ends_with("2ms"));
        assert_eq!(rendered, "    └─ ✓ Read · README.md · read 120 bytes · 2ms");
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
    fn action_labels_preserve_targets_and_unknown_tool_identity() {
        for (call, expected) in [
            ("shell $ cargo test", "Shell · $ cargo test"),
            ("list_dir docs/", "List directory · docs/"),
            ("grep 'error' in src", "Search text · 'error' in src"),
            ("custom.search docs", "custom.search · docs"),
            ("subagent inspect files", "Subagent · inspect files"),
        ] {
            let mut row = row(100, Connector::None);
            row.call = call;
            assert!(text(&row.line()).contains(expected));
        }
    }

    #[test]
    fn wide_glyphs_count_as_two_cells() {
        let mut row = row(30, Connector::None);
        row.call = "read_file 日本語のファイル名.md";
        row.detail = "";
        assert!(row.line().width() <= 30);
    }
}
