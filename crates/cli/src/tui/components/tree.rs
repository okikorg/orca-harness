//! One tree vocabulary for every rail.
//!
//! Tool rows, subagent rows, nested spawns, the queue and the todo list all
//! draw the same `├─` / `└─` branch, carry the same `│ ` stem under a row
//! that is not the last, and reserve the same connector cells when one row
//! is tied to the inspector. Keeping the glyphs and the arithmetic here
//! means a rail cannot drift from its neighbours.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::view;

/// Rows stop growing past this many cells even on a very wide terminal.
pub const ROW_CAP: usize = 132;
/// Cells a row keeps free so the selection connector has somewhere to go.
pub const CONNECTOR_RESERVE: usize = 10;

/// How a row relates to the inspector connector.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Connector {
    /// No rail selection: the row may use its whole budget.
    None,
    /// Another row in this rail is selected: keep the connector cells free
    /// so the call column stays straight across the rail.
    Reserved,
    /// This row is selected: dots run to the pane edge and end in `○`.
    Drawn,
}

impl Connector {
    pub fn for_row(rail_has_selection: bool, selected: bool) -> Self {
        match (rail_has_selection, selected) {
            (_, true) => Self::Drawn,
            (true, false) => Self::Reserved,
            (false, false) => Self::None,
        }
    }

    fn reserve(self) -> usize {
        match self {
            Self::None => 0,
            Self::Reserved | Self::Drawn => CONNECTOR_RESERVE,
        }
    }
}

/// Position of one row in its rail.
#[derive(Clone, Copy)]
pub struct TreeBranch<'a> {
    /// Everything left of the branch glyph: the rail indent plus any parent
    /// stems for nested rows.
    pub indent: &'a str,
    pub last: bool,
}

impl TreeBranch<'_> {
    pub fn glyph(&self) -> &'static str {
        if self.last {
            "└─"
        } else {
            "├─"
        }
    }

    /// `indent`, branch glyph and one space: what precedes the row body.
    pub fn prefix(&self) -> String {
        format!("{}{} ", self.indent, self.glyph())
    }

    /// What detail rows under this row start with, in place of the branch,
    /// so ownership stays readable mid-list.
    pub fn continuation(&self) -> &'static str {
        if self.last {
            "  "
        } else {
            "│ "
        }
    }

    /// Indent for a nested rail hanging off this row.
    pub fn child_indent(&self) -> String {
        format!("{}{}", self.indent, self.continuation())
    }
}

/// Cells a row body may use once the cap and the connector are honoured.
pub fn row_budget(width: usize, connector: Connector) -> usize {
    width.min(ROW_CAP).saturating_sub(connector.reserve())
}

/// Close a row: append the dotted connector when this row is the selection.
pub fn finish_row(
    mut spans: Vec<Span<'static>>,
    width: usize,
    connector: Connector,
    style: Style,
) -> Line<'static> {
    spans = super::layout::fit(Line::from(spans), row_budget(width, connector)).spans;
    if connector == Connector::Drawn && width > 0 {
        let used = view::spans_width(&spans);
        let dots = width.saturating_sub(used + 1);
        spans.push(Span::styled(
            format!(" {}○", "·".repeat(dots.saturating_sub(1).max(1))),
            style,
        ));
    }
    super::layout::fit(Line::from(spans), width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_prefix_and_continuation_agree_on_last() {
        let mid = TreeBranch {
            indent: "  ",
            last: false,
        };
        let end = TreeBranch {
            indent: "  ",
            last: true,
        };
        assert_eq!(mid.prefix(), "  ├─ ");
        assert_eq!(mid.continuation(), "│ ");
        assert_eq!(mid.child_indent(), "  │ ");
        assert_eq!(end.prefix(), "  └─ ");
        assert_eq!(end.continuation(), "  ");
    }

    #[test]
    fn reserved_and_drawn_rows_share_one_budget() {
        assert_eq!(row_budget(100, Connector::None), 100);
        assert_eq!(
            row_budget(100, Connector::Reserved),
            row_budget(100, Connector::Drawn)
        );
        assert_eq!(row_budget(400, Connector::None), ROW_CAP);
    }

    #[test]
    fn only_the_drawn_row_gets_the_connector() {
        let spans = vec![Span::raw("row")];
        let plain = finish_row(spans.clone(), 20, Connector::Reserved, Style::default());
        assert_eq!(plain.spans.len(), 1);
        let drawn = finish_row(spans, 20, Connector::Drawn, Style::default());
        assert_eq!(drawn.spans.len(), 2);
        assert!(drawn.spans[1].content.ends_with('○'));
        assert_eq!(drawn.width(), 20);
    }
}
